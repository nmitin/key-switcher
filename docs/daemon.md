# Демон X11

`layoutpilot` — первый работающий вертикальный срез. Он связывает уже готовые
части:

```
XRecord ──▶ XKB layout ──▶ char_for ──▶ CorrectionManager
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

- `doctor` проверяет XTEST, XKB, RECORD, фокус и словари.
- `run --dry-run` печатает планы, ничего не вводя.
- `run` выполняет коррекции в активном X11-окне.

Для запуска нужен `DISPLAY`. Бинарник называется `layoutpilot`.

## Что делает демон

1. XRecord приносит `KeyPress`/`KeyRelease`.
2. На каждый `KeyPress` демон обновляет XKB-группу и фокус.
3. `char_for` превращает keycode в символ с учётом Shift и Caps Lock.
4. `CorrectionManager` отдаёт план: `layout`, число Backspace, текст.
5. Демон делает Backspace, переключает XKB-группу и печатает текст через XTest.
6. События собственной инъекции записываются в очередь подавления (FR-09):
   `X11Injector` ведёт журнал отправленных `(keycode, press/release)`, а демон
   выкидывает совпадающие записи из XRecord.

Особые клавиши:

- Backspace — удаляет последний символ из буфера движка;
- Return — завершает слово без коррекции (Enter не триггер);
- Tab — завершает слово; сработает только в `aggressive` с включённой опцией;
- Escape — сбрасывает контекст.

## Профили

Пока конфига нет, используются дефолтные правила:

| Шаблон `WM_CLASS` | Профиль |
|---|---|
| `*terminal*`, `*konsole*`, `*alacritty*`, `*kitty*` | `manual` |
| `*keepass*` | `off` |
| всё остальное | `auto` |

Терминал в `manual` означает: авто-коррекция не срабатывает, ручной фикс
доступен через API, но глобальный хоткей ещё не подключён.

## Ограничения

- Порядок групп фиксирован: `us` = group 1, `ru` = group 2. Для сессии с другим
  порядком нужен конфиг/CLI-опция — следующий шаг.
- Layout state опрашивается на каждое нажатие, а не через `XkbStateNotify`.
- Нет глобальных хоткеев undo/fix/pause.
- Нет per-window раскладки.
- XRecord видит только X11/XWayland-приложения.
- Предполагается, что разделитель уже дошёл до приложения к моменту
  инъекции; на практике XTest-очередь идёт после исходного события.
- Нет конфигурации, трея и systemd-юнита.
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
RECORD: ok
dictionary: ru(привет)=177992, en(hello)=405534
result: ok
```
