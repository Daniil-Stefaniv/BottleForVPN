# BottleForApps v2

BottleForApps позволяет одновременно держать несколько разных VPN с
несовместимыми настройками на одной Windows-машине — так, что VPN-туннели
не знают друг о друге, а их трафик уходит в интернет через изначальную
(физическую) сеть компьютера. Каждая «бутылка» — конфиг-VPN (WireGuard,
OpenVPN) в собственном сетевом namespace внутри лёгкого WSL2-апплайенса:
своя таблица маршрутов, свой DNS, свой default route. Туннели поднимаются
userspace-инструментами (`wireguard-go`, `openvpn` через `/dev/net/tun`) —
без драйверов ядра, без Hyper-V-требований, работает на Windows 10/11 Home.

Наружу каждая бутылка отдаёт на `127.0.0.1`: SOCKS5-прокси (браузер и
любые приложения, умеющие прокси) и TCP-пробросы для RDP — `mstsc`
подключается на `127.0.0.1:<порт>` и попадает в сеть за VPN. Архитектура:
демон `bottled` (профили, жизненный цикл, слушатели; без прав админа) +
`bottle-agent` внутри аплаенса (root-исполнитель namespace'ов) + CLI
`bottle`. Пользовательские строки — на русском. Подробности — в
[спецификации дизайна](docs/superpowers/specs/2026-09-30-bottleforapps-v2-design.md).

## Статус: этап M1

Работает вертикальный срез env-бутылки: профиль → `bottle start` →
импорт WSL-аплаенса → namespace + WireGuard/OpenVPN-туннель (handshake
≤30/≤60 с) → SOCKS5 и TCP-пробросы на 127.0.0.1 → `bottle stop` с полным
разбором. Несколько env-бутылок одновременно.

Ещё не готово (не пытайтесь использовать):

- **host-бутылки** (фирменные GUI-клиенты AnyConnect/FortiClient на
  хосте через точечные маршруты) — этап **M2**; профиль `kind = "host"`
  при старте объяснит, что это M2;
- авто-восстановление бутылок после `wsl --shutdown`/сна — M2 (после
  перезапуска VM бутылка поднимается повторным `bottle start`);
- служба `bottle-svc`, MSI-установщик, `doctor`, `logs export` — M2/M3;
- GUI (Tauri) и macOS — M4.

Ручная приёмка этапа — [чеклист M1](docs/checklists/m1-verification.md).

## Сборка

Нужен Rust (stable, rustup) и, для аплаенса, docker (любой локальный,
например Colima).

```sh
# bottled.exe и bottle.exe (на Windows; на *nux соберётся всё, кроме работы)
cargo build --release -p daemon -p cli

# rootfs-апплайенс: Alpine + bottle-agent + wg/wireguard-go/openvpn
# => artifacts/bottleforapps-rootfs.tar.gz
bash appliance/build-rootfs.sh
```

Сборка rootfs всегда идёт под `linux/amd64` (целевая платформа WSL2).
Результат — артефакт `artifacts/bottleforapps-rootfs.tar.gz`; в CI он же
собирается джобой `appliance-build`.

## Запуск

```sh
# 1) демон в сессии пользователя (права администратора не нужны)
bottled --data-dir C:\bottles --rootfs C:\bottles\bottleforapps-rootfs.tar.gz

# 2) профиль первой бутылки: C:\bottles\bottles\office-a.toml + office-a.conf
#    (формат — спека §5.1)
```

Пример профиля:

```toml
id = "office-a"
name = "Офис А"
kind = "env"

[vpn]
protocol = "wireguard"          # или "openvpn"
config = "office-a.conf"        # копия конфига рядом с профилем

[expose]
socks5 = 0                      # 0 = автоподбор порта
forwards = [{ listen = 0, target = "10.20.30.15:3389" }]  # RDP
# dns = "10.20.30.1"            # опционально: DNS внутри namespace
```

Работа с бутылками:

```sh
bottle list                    # профили в хранилище
bottle start office-a          # импорт аплаенса + туннель + слушатели
bottle status                  # состояние и фактические порты на 127.0.0.1
bottle show office-a           # профиль + рантайм
bottle stop office-a           # разбор namespace, порты освобождаются
```

Использование: браузер → SOCKS5 `127.0.0.1:<порт из status>`; RDP →
`mstsc /v:127.0.0.1:<порт проброса>`.

## Диагностика

- Журнал демона — вывод `bottled` в консоли (уровень — `RUST_LOG`,
  по умолчанию `info`); каталог `<DataDir>\logs` зарезервирован и
  заполняется на этапе M2.
- Внутри аплаенса: `wsl -d bottleforapps -- ip netns list` (живые
  бутылки), `... -- ss -ltn` (мосты агента), `... -- ip netns exec
  bottle-office-a wg show` (handshake туннеля).
- Пошаговая проверка «двух туннелей и RDP» —
  [чеклист приёмки M1](docs/checklists/m1-verification.md).

## Документы

- [Спецификация дизайна v2](docs/superpowers/specs/2026-09-30-bottleforapps-v2-design.md) —
  архитектура, протоколы, надёжность, дорожная карта.
- [План этапа M0+M1](docs/superpowers/plans/2026-09-30-bottleforapps-v2-m0-m1.md) —
  задачи и результаты текущего этапа.
- [Сборка аплаенса](appliance/README.md).

## CI

`rust-checks` / `windows-checks` (fmt, clippy, test), `agent-integration`
(netns/WG под root на Linux), `appliance-build` (артефакт rootfs) и
`wsl-integration` — полный срез на windows-раннере с WSL2 (в M1 —
`continue-on-error`, спека §15: фолбэк — Linux-тесты агента).
