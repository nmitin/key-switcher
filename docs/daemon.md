# Демон

`layoutpilot` — вертикальный срез. Он связывает уже готовые части:

```
XRecord/evdev ──▶ layout provider ──▶ char_for ──▶ CorrectionManager
                                                   │
                                                   ▼
                                       CorrectionPlan (layout, Backspace, text)
                                                   │
                                                   ▼
                              switch_to + XTest/uinput backspace/text
```

## Команды

```sh
cargo run -p layoutpilot-daemon -- doctor
cargo run -p layoutpilot-daemon -- run --dry-run
cargo run -p layoutpilot-daemon -- run
```

- `doctor` проверяет выбранные capture/inject/layout-бэкенды, фокус и словари.
- `--config PATH` задаёт файл конфигурации; по умолчанию
  `$XDG_CONFIG_HOME/layoutpilot/config.toml` или `~/.config/layoutpilot/config.toml`.
- `run --dry-run` печатает планы, ничего не вводя.
- `run` выполняет коррекции в активном окне.

На X11 нужен `DISPLAY` и XTest. На Wayland нужен доступ к `/dev/input/event*`
и `/dev/uinput`: демон читает evdev и печатает через uinput. Установите
[`packaging/60-layoutpilot.rules`](../packaging/60-layoutpilot.rules) или
настройте ACL. Бинарник называется `layoutpilot`.

На X11/KDE XWayland при `inject.request_permission_on_start = true` демон при
старте делает один Shift-tap, чтобы KDE показал запрос на управление вводом
сразу, а не в момент первой коррекции. Для uinput этот запрос не нужен.
В `--dry-run` preflight не выполняется.

## Что делает демон

1. Выбранный capture-backend (`xrecord` или `evdev`) приносит key-события.
2. На каждый `KeyPress` демон обновляет активную раскладку и фокус. Для evdev
   модификаторы (Shift/Ctrl/Alt/Super/CapsLock) отслеживаются вручную.
3. `char_for` превращает keycode в символ с учётом Shift и Caps Lock.
4. `CorrectionManager` отдаёт план: `layout`, число Backspace, текст.
5. Демон выдерживает `pre_inject_delay_ms` (uinput), делает Backspace,
   переключает раскладку и печатает текст.
   - X11: XTest; на KDE — D-Bus `org.kde.keyboard` + синхронизация XKB-группы.
   - Wayland: uinput; на KDE — D-Bus, на Sway/Hyprland — IPC композитора.
   Разделитель удаляется вместе со словом и печатается обратно отдельным шагом
   после `separator_delay_ms` (по умолчанию 20 мс); перед редактированием
   выдерживается `layout_settle_ms` (для uinput — не меньше 100 мс, ADR-018).
6. События собственной инъекции подавляются (FR-09): XTest ведёт журнал
   отправленных `(keycode, press/release)`, а evdev-путь игнорирует устройство
   `LayoutPilot Virtual Keyboard` и дополнительно проверяет имя устройства.

Особые клавиши:

- Backspace — если была коррекция и `backspace_undo = true`, отменяет её;
  иначе удаляет последний символ;
- Return — триггер коррекции при `triggers = ["space", "enter"]` (по умолчанию);
  коррекция выполняется на отпускании Enter; synthetic Return игнорируется
  300 мс, чтобы не запустить повторную коррекцию;
- Tab — завершает слово; сработает только в `aggressive` с включённой опцией;
- Escape — сбрасывает контекст.

## Глобальные хоткеи

Берутся из `[general]`:

```toml
undo_hotkey = "Pause"
fix_hotkey = "Ctrl+F8"
pause_hotkey = "Ctrl+F9"
```

- undo — отменяет последнюю коррекцию; повторное нажатие возвращает её
  (redo), пока не было нового ввода/смены фокуса;
- fix — вручную конвертирует текущее слово (в том числе в `manual`);
- pause — ставит/снимает паузу, очищая буфер и undo.

Хоткеи сопоставляются по физическому keycode и состоянию модификаторов;
Caps Lock и Num Lock игнорируются. Поддерживаются одиночные символы,
`Pause`, `Escape`, `Space`, `Tab`, `Enter`, `Backspace`, `F1`–`F12`.

На X11 демон дополнительно захватывает хоткеи через `XGrabKey` с вариантами
Caps Lock/Num Lock. Если захват удался, активное приложение нажатие не получает.
Если комбинация уже занята другим клиентом (`BadAccess`), демон пишет
предупреждение и оставляет этот хоткей пассивным: он сработает, но приложение
его тоже увидит.

На Wayland `XGrabKey` не используется: он работает только с XWayland и может
«успешно» захватить клавишу, которую нативное окно всё равно не отдаст. Хоткеи
сопоставляются в evdev-потоке; приложение тоже может их увидеть. Основной
ручной жест — двойной Shift (последнее слово) и повторный двойной Shift после
коррекции для отмены.

## KDE: раскладка на окно

Если в KDE включён `SwitchMode=Window`, у каждого окна своя раскладка. После
коррекции демон переключает раскладку **целевого окна**; когда фокус уходит в
терминал, KDE показывает раскладку терминала. Поэтому проверять индикатор и
текст нужно в самом Kate, а не в терминале с логом.

## Звук

При `general.sound_on_layout_change = true` звук проигрывается при смене
раскладки: после коррекции и при ручном переключении. По умолчанию используется
системный `camera-shutter.oga`; путь задаётся `general.sound_file`.
Используются `paplay` / `pw-play` / `canberra-gtk-play`; звук не блокирует
цикл обработки. При запуске звука демон печатает строку
`layout sound via ...`; если её нет, звук не вызывался (например, раскладка уже
была целевой).

## Профили

Правила берутся из `[apps]` конфига. Встроенные значения:

| Шаблон `WM_CLASS` | Профиль |
|---|---|
| `*terminal*`, `*konsole*`, `*alacritty*`, `*kitty*` | `manual` |
| `*keepass*` | `off` |
| всё остальное | `auto` |

Терминал в `manual` означает: авто-коррекция не срабатывает, ручной фикс
доступен через API, но глобальный хоткей ещё не подключён.

## Проверено вживую

В KDE Wayland:

- `QT_QPA_PLATFORM=xcb kate` — Kate становится XWayland-клиентом. Демон видит
  фокус и нажатия, коррекция `ghbdtn ` → `привет ` работает через XTest.
- `kate` без переменной и `geany` — нативные Wayland-клиенты. XRecord их не
  видит; для них нужен путь evdev-capture + uinput-инъекция. Код этого пути
  добавлен, но в среде разработки `/dev/uinput` недоступен, поэтому живой
  прогон на нативном окне ещё нужен.

На X11 поведение не изменилось: XRecord + XTest + XKB.

`doctor` в KDE Wayland без uinput показывает:

```text
session: wayland
uinput: FAIL (uinput I/O error: No such file or directory)
KDE layouts: ok (current=Some(Ru), list=[("us", Some(Us)), ("ru", Some(Ru))])
capture: FAIL (/dev/input does not exist)
```

После установки udev-правила и появления доступа к `/dev/input` и `/dev/uinput`
обе строки должны стать `ok`.

## Ограничения

- Layout state опрашивается на каждое нажатие, а не через события
  `XkbStateNotify`/`layoutChanged`; Sway/Hyprland дёргают IPC на каждое событие.
- evdev-устройства перечисляются один раз при старте: hotplug не реализован.
- GNOME-провайдер раскладки не реализован; на GNOME `layout = "auto"` уходит
  в X11/XWayland.
- KDE: вариант раскладки (`ru+phonetic`) и `layoutChanged` пока не читаются.
- uinput создаёт новое устройство: в per-device композиторах у него может быть
  своя группа, а античиты/игры видят его как клавиатуру. `allow_uinput_in_games`
  читается, но детекта игр ещё нет.
- Порядок групп задаётся в `[layout] order`, но автоопределение групп ещё
  не сделано.
- При неудачном `XGrabKey` (конфликт с другим клиентом) хоткей остаётся
  пассивным: приложение тоже получит нажатие.
- Нет per-window раскладки.
- XRecord видит только X11/XWayland-приложения.
- Предполагается, что разделитель уже дошёл до приложения к моменту
  инъекции; на практике XTest-очередь идёт после исходного события.
- Нет hot reload конфига, трея и systemd-юнита; конфиг читается при старте.
- Звук проверен по коду и доступности плееров, но не прослушан в этой сессии.
- `--dry-run` обновляет состояние менеджера, но не переключает раскладку:
  это диагностический режим, а не полноценная сессия.

## Проверка

```sh
cargo test --workspace
cargo run -p layoutpilot-daemon -- doctor
```

Для Wayland-сессии `doctor` проверяет uinput, KDE/Sway/Hyprland IPC и evdev;
для X11 — XTEST, XKB и XRecord.
