# Установка LayoutPilot

## 1. Сборка

```sh
cargo build --release -p layoutpilot-daemon
install -Dm755 target/release/layoutpilot ~/.local/bin/layoutpilot
```

## 2. Доступ к клавиатуре и uinput

```sh
sudo install -m 644 packaging/60-layoutpilot.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules
sudo udevadm trigger
```

Проверка:

```sh
getfacl /dev/uinput
getfacl /dev/input/event*
```

У пользователя должны появиться права `user:<name>:rw-`.

## 3. Автозапуск

```sh
install -Dm644 packaging/layoutpilot.service ~/.config/systemd/user/layoutpilot.service
systemctl --user daemon-reload
systemctl --user enable --now layoutpilot.service
```

Проверка:

```sh
systemctl --user status layoutpilot.service
journalctl --user -u layoutpilot.service -f
```

После изменения `~/.config/layoutpilot/config.toml`:

```sh
systemctl --user restart layoutpilot.service
```

Остановить и убрать из автозапуска:

```sh
systemctl --user disable --now layoutpilot.service
```

Служба запускается после `graphical-session.target`. Если бинарник лежит не в
`~/.local/bin/layoutpilot`, поправь `ExecStart` в unit-файле.

## Пакет .deb (Debian, Ubuntu, Linux Mint)

```sh
packaging/deb/build-deb.sh   # -> target/deb/layoutpilot_<версия>_amd64.deb
```

Бинарник собирается в контейнере `ubuntu:22.04` (нужен Docker), поэтому
требует glibc ≥ 2.34 и работает на Ubuntu 22.04+, Mint 21+, Debian 12+.
Пакет ставит `/usr/bin/layoutpilot`, автозапуск в `/etc/xdg/autostart`
(Cinnamon, MATE, Xfce не поднимают `graphical-session.target`) и
user-юнит systemd (не включён). Инструкция для пользователя —
[`deb/README.Debian`](deb/README.Debian). Второй экземпляр демона сразу
завершается, так что автозапуск и юнит не конфликтуют.
