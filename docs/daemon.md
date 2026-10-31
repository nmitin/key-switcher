# Демон

`layoutpilot` — первый работающий вертикальный срез. Он связывает уже готовые
части:

```
XRecord/evdev ──▶ XKB layout ──▶ char_for ──▶ CorrectionManager
                                              │
                                              ▼
                                  CorrectionPlan (layout, Backspace, text)
                                              │
                                              ▼
                              XKB switch_to + XTest backspace/text
```

## Команды

```sh
cargo run -p layoutpilot-daemon -- doctor
cargo run -p layoutpilot-daemon -- run --dry-run
cargo run -p layoutpilot-daemon -- run
```

- `doctor` проверяет XTEST, XKB, RECORD/evdev, фокус и словари.
- `--config PATH` задаёт файл конфигурации; по умолчанию
  `$XDG_CONFIG_HOME/layoutpilot/config.toml` или `~/.config/layoutpilot/config.toml`.
- `run --dry-run` печатает планы, ничего не вводя.
- `run` выполняет коррекции в активном X11-окне.

Для X11-инъекции нужен `DISPLAY`. Бинарник называется `layoutpilot`.
evdev-capture можно включить через `[capture] backend = "evdev"`; для этого
нужен доступ к `/dev/input/event*`.

## Что делает демон

1. Выбранный capture-backend (`xrecord` или `evdev`) приносит key-события.
2. На каждый `KeyPress` демон обновляет XKB-группу и фокус. Для evdev
   модификаторы (Shift/Ctrl/Alt/Super/CapsLock) отслеживаются вручную.
3. `char_for` превращает keycode в символ с учётом Shift и Caps Lock.
4. `CorrectionManager` отдаёт план: `layout`, число Backspace, текст.
5. Демон делает Backspace, переключает XKB-группу и печатает текст через XTest.
6. События собственной инъекции записываются в очередь подавления (FR-09):
   `X11Injector` ведёт журнал отправленных `(keycode, press/release)`, а демон
   выкидывает совпадающие записи из capture-потока.

Особые клавиши:

- Backspace — удаляет последний символ из буфера движка;
- Return — завершает слово без коррекции (Enter не триггер);
- Tab — завершает слово; сработает только в `aggressive` с включённой опцией;
- Escape — сбрасывает контекст.

## Профили

Правила берутся из `[apps]` конфига. Встроенные значения:

| Шаблон `WM_CLASS` | Профиль |
|---|---|
| `*terminal*`, `*konsole*`, `*alacritty*`, `*kitty*` | `manual` |
| `*keepass*` | `off` |
| всё остальное | `auto` |

Терминал в `manual` означает: авто-коррекция не срабатывает, ручной фикс
доступен через API, но глобальный хоткей ещё не подключён.

## Ограничения

- Layout state опрашивается на каждое нажатие, а не через `XkbStateNotify`.
- evdev-устройства перечисляются один раз при старте: hotplug не реализован.
- evdev не даёт состояние раскладки; группа берётся из XKB (X11). На чистом
  Wayland без XWayland этого пути пока нет.
- Порядок групп задаётся в `[layout] order`, но автоопределение групп ещё
  не сделано.
- Нет глобальных хоткеев undo/fix/pause.
- Нет per-window раскладки.
- XRecord видит только X11/XWayland-приложения.
- Предполагается, что разделитель уже дошёл до приложения к моменту
  инъекции; на практике XTest-очередь идёт после исходного события.
- Нет hot reload конфига, трея и systemd-юнита; конфиг читается при старте.
- `--dry-run` обновляет состояние менеджера, но не переключает раскладку:
  это диагностический режим, а не полноценная сессия.

## Проверка

```sh
cargo test -p layoutpilot-daemon
DISPLAY=:1 cargo run -p layoutpilot-daemon -- doctor
```

`doctor` на XWayland в этом окружении:

```text
XTEST: ok
XKB: ok (group 0, active Some(Us))
focus: window=0x200000, app=None
capture: ok (xrecord)
dictionary: ru(привет)=177992, en(hello)=405534
result: ok
```
