# Апплайенс BottleForApps (Alpine rootfs)

Корневая файловая система для WSL2-дистрибутива, в котором живёт `bottle-agent`.
После `wsl --import` агент стартует автоматически через boot-команду из
`/etc/wsl.conf` и поднимает control-сервер на `0.0.0.0:7777`.

## Состав

- **Alpine 3.22** (x86_64/musl) — базовый rootfs.
- **Пакеты**: `wireguard-go`, `wireguard-tools`, `openvpn`, `iproute2`, `iptables`.
- **`/bin/bottle-agent`** — агент (крейт `agent`, target `x86_64-unknown-linux-musl`),
  подкоманды `serve` (с флагом `--boot`) и `relay`.
- **`/etc/wsl.conf`** — boot-команда WSL:

  ```ini
  [boot]
  command = /bin/bottle-agent serve --boot
  ```

- **`/etc/bottle-appliance-version`** — версия workspace (генерируется
  `build-rootfs.sh` из `[workspace.package].version` корневого `Cargo.toml`;
  совпадает с `CARGO_PKG_VERSION` агента в `HelloAck.appliance_version`).

## Сборка

```bash
./appliance/build-rootfs.sh
```

Скрипт (требуется docker; локально — Colima, в CI — ubuntu): генерирует
version-файл, собирает multi-stage образ `bottleforapps-appliance:latest`
под `--platform linux/amd64`, гоняет smoke-проверку и экспортирует rootfs
контейнера в **`artifacts/bottleforapps-rootfs.tar.gz`** (путь и размер
печатается в конце). CI-джоба `appliance-build` загружает артефакт тем же
скриптом.

## Ручная проверка на Windows

```powershell
wsl --import bottleforapps <каталог-для-distro> artifacts\bottleforapps-rootfs.tar.gz
wsl -d bottleforapps -- cat /etc/bottle-appliance-version
# control-сервер: wsl -d bottleforapps -- netstat -tln  (0.0.0.0:7777)
wsl --unregister bottleforapps   # удалить по окончании
```

Токен control-протокола лежит в дистрибутиве в `/etc/bottle-agent/token`.
