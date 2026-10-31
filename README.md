# LayoutPilot

Автопереключатель раскладки клавиатуры для Linux. Проект находится на раннем
этапе: готовы платформенно-независимое ядро, движок и словарный скоринг;
платформенные бэкенды (evdev, X11, Wayland) ещё не реализованы.

- ТЗ: [`tz-layout-switcher-v0.2.md`](tz-layout-switcher-v0.2.md)
- Решения: [`docs/decisions.md`](docs/decisions.md)
- Программа спайков: [`docs/stage-0-checklist.md`](docs/stage-0-checklist.md)
- Движок: [`docs/engine.md`](docs/engine.md)
- Словари и скоринг: [`docs/dictionaries.md`](docs/dictionaries.md)

## Что уже работает

`layoutpilot-core` — чистая библиотека без I/O и системных вызовов:

- таблицы `us`/`ru`, сгенерированные из XKB (118 буквенных привязок, 189 записей с пунктуацией);
- конвертация переинтерпретацией физических клавиш: `ghbdtn` → `привет`;
- сохранение регистра, пунктуации, цифр и пробелов;
- строгая проверка клавиатурной обратимости для слов;
- сегментация слов и кольцевой буфер символов;
- property-тесты инволютивности и сохранения регистра.

`layoutpilot-core::Engine` — платформенно-независимый движок принятия решений:

- триггеры: пробел и закрывающая пунктуация; Enter не триггер, Tab — только
  в `aggressive` с явным включением;
- профили `auto` / `aggressive` / `conservative` / `manual` / `off` и
  резолвер `app_id` → профиль (точное совпадение → glob → `*`);
- защитные правила: короткие слова, цифры рядом со словом, URL/email/пути,
  Ctrl/Alt/Super, активный IME, недостоверный layout state;
- `Detector`-трейт для вероятности кандидата;
- undo/redo с инвалидацией при любом новом вводе или сбросе контекста (FR-04);
- `Action::Replace/Undo/Redo` с числом Backspace и текстом для инжектора.

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
```

Требуется Rust 1.85+ и системные XKB-данные только для перегенерации таблиц.

## Структура

```
crates/
  layoutpilot-layouts/   таблицы XKB и физические привязки
  layoutpilot-core/      конвертация, сегментация, буфер, движок
  layoutpilot-dict/      FST-словари, биграммы, DictionaryDetector
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
- Нет платформенных бэкендов (evdev, X11, Wayland), менеджера инъекции и
  реального undo-хука в приложении.
- Резолвер профилей есть, но интеграция с фокусом/`app_id` появится вместе с
  платформенным слоем.

## Лицензия

Код — MIT OR Apache-2.0, см. [`LICENSE-MIT`](LICENSE-MIT) и
[`LICENSE-APACHE`](LICENSE-APACHE). Данные словарей — отдельная работа под
**CC BY-SA 4.0**: [`data/dict/README.md`](data/dict/README.md),
[`data/dict/LICENSE-CC-BY-SA-4.0`](data/dict/LICENSE-CC-BY-SA-4.0).
