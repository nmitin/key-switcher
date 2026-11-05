# LayoutPilot

Автопереключатель раскладки клавиатуры для Linux. Проект находится на раннем
этапе: готовы платформенно-независимое ядро, движок, словарный скоринг,
X11-демон, evdev-capture, uinput-инъекция и конфиг; провайдеры раскладки есть
для KDE, Sway и Hyprland.

- ТЗ: [`tz-layout-switcher-v0.2.md`](tz-layout-switcher-v0.2.md)
- Решения: [`docs/decisions.md`](docs/decisions.md)
- Программа спайков: [`docs/stage-0-checklist.md`](docs/stage-0-checklist.md)
- Движок: [`docs/engine.md`](docs/engine.md)
- Словари и скоринг: [`docs/dictionaries.md`](docs/dictionaries.md)
- Демон: [`docs/daemon.md`](docs/daemon.md)
- Конфиг: [`docs/config.md`](docs/config.md)

## Что уже работает

`layoutpilot-core` — чистая библиотека без I/O и системных вызовов:

- таблицы `us`/`ru`, сгенерированные из XKB (118 буквенных привязок, 196 записей с пунктуацией);
- конвертация переинтерпретацией физических клавиш: `ghbdtn` → `привет`;
- сохранение регистра, пунктуации, цифр и пробелов;
- строгая проверка клавиатурной обратимости для слов;
- сегментация слов и кольцевой буфер символов;
- пунктуация-буква внутри слова: `cfk.n` → `салют`, `cvt[` → `смех`;
- property-тесты инволютивности и сохранения регистра.

`layoutpilot-core::Engine` — платформенно-независимый движок принятия решений:

- триггеры: по умолчанию пробел и Enter; пунктуация сохраняется в слове,
  Tab — только в `aggressive` с явным включением;
- профили `auto` / `aggressive` / `conservative` / `manual` / `off` и
  резолвер `app_id` → профиль (точное совпадение → glob → `*`);
- защитные правила: короткие слова, цифры рядом со словом, URL/email/пути,
  Ctrl/Alt/Super, активный IME, недостоверный layout state;
- `Detector`-трейт для вероятности кандидата;
- undo/redo с инвалидацией при любом новом вводе или сбросе контекста; Backspace
  сразу после коррекции отменяет её (`backspace_undo`);
- `Action::Replace/Undo/Redo` с числом Backspace и текстом для инжектора.

`layoutpilot-daemon` — исполняемый вертикальный срез:

- XRecord/evdev → нормализация → `Engine`/`DictionaryDetector` → XTest/uinput
  + переключение раскладки;
- `doctor` для проверки выбранных capture/inject/layout-бэкендов, фокуса и словарей;
- `run --dry-run` для просмотра планов без инъекции;
- конфиг `~/.config/layoutpilot/config.toml` или `--config PATH`;
- порядок XKB-групп задаётся в `[layout] order`;
- подавление собственных событий XTest и uinput;
- профили по `WM_CLASS` (терминалы — `manual`, KeePass — `off`);
- глобальные хоткеи undo/fix/pause из конфига; на X11 — с `XGrabKey`;
- на X11 — startup-запрос XTest-разрешения одним Shift-tap;
- KDE: переключение раскладки через D-Bus + XKB-синхронизация;
- Sway/Hyprland: переключение раскладки через IPC композитора;
- звуковой индикатор смены раскладки (`sound_on_layout_change`, по умолчанию
  системный звук затвора);
- разделитель удаляется вместе со словом и вставляется отдельным шагом:
  первая буква не остаётся, последний пробел не пропадает.

`layoutpilot-inject` — Wayland-инъекция:

- uinput-клавиатура `LayoutPilot Virtual Keyboard`;
- перевод текста в физические keycode+Shift по таблице раскладки;
- Backspace, Enter и Tab;
- отпускание всех клавиш при ошибке и на `Drop`.

`layoutpilot-config` — TOML-конфиг:

- секции `[general]`, `[capture]`, `[privacy]`, `[inject]`, `[layout]`,
  `[ime]`, `[apps]`;
- значения по умолчанию и валидация;
- `[apps]` → `ProfileResolver`, `[general]` → `EngineConfig`;
- пример: [`config.example.toml`](config.example.toml).

`layoutpilot-capture` — evdev:

- перечисление `/dev/input/event*` и фильтр клавиатур по letter-keys;
- пропуск собственной uinput-клавиатуры по имени;
- неблокирующее чтение в отдельном потоке на устройство;
- evdev keycode → XKB keycode (`+8`);
- hotplug пока не поддерживается.

`layoutpilot-compositor` — раскладка на уровне композитора:

- KDE Plasma D-Bus `org.kde.keyboard` / `org.kde.KeyboardLayouts`;
- Sway IPC: `GET_INPUTS`, `xkb_switch_layout`, свой XKB-индекс;
- Hyprland IPC: `j/devices`, `j/getoption input:kb_layout`,
  `switchxkblayout all N`;
- список раскладок и `setLayout` по коду `us`/`ru`;
- синхронизация XKB-группы для XWayland-инъекции.

`layoutpilot-x11` — первый платформенный срез:

- XTest-инъекция (keycode, Shift, Backspace, текст по таблице);
- XKB: текущая группа, активная раскладка, переключение;
- XRecord-захват глобальных KeyPress/KeyRelease;
- активное окно, `WM_CLASS` и заголовок;
- нормализация keycode → символ с учётом Shift/CapsLock;
- `XGrabKey`-перехват горячих клавиш с вариантами Lock/NumLock.

`layoutpilot-dict` — словари и production-детектор:

- FST-словари ru/en по 50k слов из FrequencyWords;
- char-биграммы с маркерами границ слова;
- сглаженный score: `log P_dict + ngram_weight · avg(log P_bigram)`
  + пользовательские частоты;
- `DictionaryDetector` реализует `Detector` и работает с движком без изменений;
- консервативное правило: если кандидата нет в словаре, уверенность 0;
  если исходное слово известно — нужен сильный перевес (`known_bias`).

```rust
use layoutpilot_core::{Engine, EngineConfig, InputContext, LayoutId, Profile};
use layoutpilot_dict::DictionaryDetector;

let mut engine = Engine::new(EngineConfig::default(), DictionaryDetector::new());
let context = InputContext::new(Profile::Auto, LayoutId::Us);
for ch in "ghbdtn ".chars() {
    if let Some(action) = engine.on_char(ch, &context) {
        println!("{action:?}");
    }
}
```

Семь русских букв (`х ъ ж э б ю ё`) находятся на клавишах пунктуации. При
конвертации в US они дают `[ ; ; ' , . \``, поэтому `convert_word` отвергает
такие слова: это физически корректно, но не является словом-кандидатом.
`reinterpret` показывает физический результат без такой проверки.

## Быстрый старт

```sh
cargo test --workspace
cargo run -p layoutpilot-core --example convert -- us ru "ghbdtn"
# привет
cargo run -p layoutpilot-core --example convert -- ru us "привет"
# ghbdtn
cargo run -p layoutpilot-core --example engine -- "ghbdtn "
# replace "ghbdtn" -> "привет": Backspace x7, insert "привет "
cargo run -p layoutpilot-dict --example detect -- ghbdtn привет
cargo run -p layoutpilot-dict --example calibrate -- 5000
cargo run -p layoutpilot-daemon -- doctor
cargo run -p layoutpilot-daemon -- doctor --config config.example.toml
cargo run -p layoutpilot-daemon -- run --dry-run
```

Требуется Rust 1.85+ и системные XKB-данные только для перегенерации таблиц.

Для Wayland-сессии нужен доступ к `/dev/input/event*` и `/dev/uinput`:

```sh
sudo install -m 644 packaging/60-layoutpilot.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules && sudo udevadm trigger
```

После этого `layoutpilot doctor` должен показать `uinput: ok` и
`capture: ok (evdev)`. На X11 достаточно `DISPLAY` и XTest.

## Структура

```
crates/
  layoutpilot-layouts/   таблицы XKB и физические привязки
  layoutpilot-core/      конвертация, сегментация, буфер, движок
  layoutpilot-dict/      FST-словари, биграммы, DictionaryDetector
  layoutpilot-x11/       XTest, XRecord, XKB state, фокус
  layoutpilot-inject/    uinput-инъекция для Wayland
  layoutpilot-compositor/KDE D-Bus, Sway и Hyprland IPC
  layoutpilot-config/    TOML, валидация, профили, порядок раскладок
  layoutpilot-capture/   evdev-захват клавиатур
  layoutpilot-daemon/    бинарник layoutpilot: capture → engine → inject
packaging/
  60-layoutpilot.rules   udev uaccess для /dev/input и /dev/uinput
config.example.toml      пример конфигурации
data/
  xkb/                   сгенерированные таблицы и их происхождение
  dict/                  FrequencyWords (CC BY-SA 4.0), FST собирается из них
tools/gen_xkb_tables.py  генератор таблиц из xkbcli
docs/                    ТЗ, ADR, чек-лист спайков, движок, словари
```

## Перегенерация таблиц

На машине с `xkbcommon-tools`:

```sh
python3 tools/gen_xkb_tables.py
cargo test --workspace
```

Генератор фиксирует версию `xkbcli` в `data/xkb/README.md`, а таблицы попадают в
`crates/layoutpilot-layouts/src/generated.rs` и `data/xkb/us_ru.tsv`.

## Ограничения текущего среза

- Только пары `us`/`ru`; другие раскладки — по мере расширения таблиц.
- Словари — топ-50k, без морфологии; редкие и профессиональные слова
  считаются неизвестными.
- Нет word-bigram контекста и layout prior; калибровка температуры и
  `known_bias` предварительная (этап 3).
- Есть Wayland-инъекция через uinput и провайдеры KDE/Sway/Hyprland, но нет
  EIS и `virtual-keyboard-v1`. uinput создаёт отдельную клавиатуру: в играх и
  per-device композиторах это надо учитывать.
- GNOME-провайдер раскладки не реализован; на GNOME `layout = "auto"` уходит
  в X11/XWayland.
- KDE: вариант раскладки (`ru+phonetic`) и сигнал `layoutChanged` пока не
  читаются; layout state опрашивается на каждое нажатие.
- Sway/Hyprland-провайдеры покрыты тестами с fake IPC, но живьём ещё не
  проверялись; в per-device композиторах у uinput-устройства может быть своя
  группа раскладки.
- XRecord видит только X11/XWayland-приложения. На Wayland `capture = "auto"`
  теперь выбирает evdev, поэтому нативные окна тоже видны.
- evdev перечисляет устройства один раз при старте: hotplug не реализован.
- Порядок XKB-групп задаётся в конфиге, но автоопределение групп ещё не сделано.
- При конфликте `XGrabKey` хоткей остаётся пассивным: приложение тоже получает
  нажатие.
- Startup-запрос разрешения рассчитан на XTest/KDE XWayland; для uinput он не
  нужен.
- Нет hot reload конфига и per-window раскладки.
- Обучение и LLM-поля конфига читаются, но пока не влияют на поведение.

## Лицензия

Код — MIT OR Apache-2.0, см. [`LICENSE-MIT`](LICENSE-MIT) и
[`LICENSE-APACHE`](LICENSE-APACHE). Данные словарей — отдельная работа под
**CC BY-SA 4.0**: [`data/dict/README.md`](data/dict/README.md),
[`data/dict/LICENSE-CC-BY-SA-4.0`](data/dict/LICENSE-CC-BY-SA-4.0).
