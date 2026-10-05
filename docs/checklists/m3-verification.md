# Чеклист релизной приёмки M3

Критерий (спека §14): **чистая машина: установка → первая бутылка без
ручных шаманств**. Проверяется на чистой Windows-VM (10/11, подходит
Home). Каждый пункт: Шаги / Ожидаемый результат / Где смотреть.

Использовать свежие артефакты релиза: `BottleForApps-<версия>-win-x64.msi`
и `BottleForApps-portable-<версия>-win-x64.zip` (+ SHA256SUMS.txt).

---

## 1. MSI на чистую машину

**Шаги**
1. Чистая VM без WSL: проверить `wsl --status` (ошибка — норма для пункта).
2. Запустить MSI (двойной клик) → один запрос UAC.
3. Если установщик включал WSL и система просит перезагрузку — перезагрузить.
4. Открыть **новое** окно PowerShell, запустить `bottled`.
5. `bottle doctor`.

**Ожидаемый**
- UAC ровно один; перезагрузка — только если WSL только что включился.
- `wsl --status` после перезагрузки отвечает.
- `bottle --version` (из любого каталога — PATH), `bottled` стартует,
  сам импортирует аплаенс при первом `start`.
- `doctor`: `wsl` ok; `wsl-networking` отсутствует (нет mirrored);
  `appliance` warn «не импортирован — нормально до первого старта»
  или ok после первого старта; `service` ok (служба из MSI);
  `agent` warn/ok; `profiles` ok; `ports` ok. **Ни одного ✗.**

**Где смотреть**: `bottle doctor`; службы: `sc.exe query
BottleForAppsRouteHelper` → RUNNING; файлы: `C:\Program Files\
BottleForApps\` (bottle/bottled/bottle-svc/appliance).

## 2. Первая env-бутылка (WireGuard)

**Шаги**
1. `bottle create office-a --name "Офис А" --kind env --protocol wireguard --config C:\wg\office-a.conf --forward <RDP-IP>:3389 --dns <DNS-VPN>`.
2. `bottle start office-a`.
3. Подключиться `mstsc /v:127.0.0.1:<порт из status>`.
4. `bottle status`, затем `bottle stop office-a`.

**Ожидаемый**
- `create` печатает «создана»; конфиг скопирован в
  `%LOCALAPPDATA%\BottleForApps\bottles\office-a.conf`.
- `start` без ручных действий импортирует аплаенс и поднимает туннель;
  RDP-сессия работает; `stop` полностью разбирает.

**Где смотреть**: `bottle status`; трассировка endpoint'а через
физику (`bottle doctor` после старта: appliance/agent ok).

## 3. Host-бутылка с реальным клиентом

**Шаги**
1. Служба уже установлена MSI — проверить `sc.exe query
   BottleForAppsRouteHelper`.
2. Установить фирменный VPN-клиент (например, любой имеющийся).
3. `bottle create corp --name "Корп" --kind host --vpn <путь к exe>
   --endpoint <host> [--vpn-interface <имя адаптера>]`.
4. `bottle start corp` → дождаться адаптера → `bottle status`.

**Ожидаемый**: SOCKS5/пробросы host-бутылки работают параллельно с
env-бутылкой из п.2; точечные маршруты видны в
`%PROGRAMDATA%\BottleForApps\routes.jsonl`; `bottle stop corp`
предупреждает, что клиент оставлен запущенным.

## 4. Журналы и экспорт

**Шаги**
1. `bottle logs export`.
2. Открыть ZIP: `logs/*`, `svc/*` (если служба работала), `system.txt`.

**Ожидаемый**
- ZIP в каталоге данных; путь напечатан; предупреждение про
  IP/имена/endpoint'ы напечатано.
- `system.txt` содержит версии демона/протокола/аплаенса, состояния
  бутылок, профили; **секретов нет** — визуально проверить: приватных
  ключей WireGuard, содержимого конфигов и аргументов клиентов в ZIP
  не найти (аргументы маскированы `<аргументы скрыты>`).

**Где смотреть**: содержимое ZIP; каталог `<data>\logs` (файлы
`bottled.YYYY-MM-DD`).

## 5. Upgrade на битый/старый аплаенс

**Шаги**
1. Имитация несовместимости: подменить rootfs старой версией
   (или `wsl --unregister bottleforapps` + положить в
   `<data>\appliance\` старый tar.gz) → `bottle start`.
2. При ошибке несовместимости — `bottle upgrade`.
3. `bottle start office-a` снова.

**Ожидаемый**
- Ошибка handshake версии предлагает `bottle upgrade`.
- `upgrade` при поднятых бутылках отказывает («остановите бутылки»).
- После `upgrade` — ответ со старой/новой версией; токен
  пересоздан (файл `appliance-token` обновился); бутылка поднимается.

**Где смотреть**: вывод CLI; `%LOCALAPPDATA%\BottleForApps\
appliance-token` (время изменения); `wsl -l -q`.

## 6. Удаление MSI

**Шаги**
1. «Панель управления → Приложения → BottleForApps» → удалить.
2. Проверить: каталог `C:\Program Files\BottleForApps`, PATH
   (`$env:Path` в новом окне), `sc.exe query
   BottleForAppsRouteHelper`.

**Ожидаемый**: файлов нет; PATH очищен; служба удалена; каталог
данных с профилями остался (сознательно).

## 7. Portable без админа

**Шаги**
1. На машине с включённым WSL распаковать portable-zip в каталог
   без прав записи в Program Files.
2. Запустить `bottled.exe`, `bottle doctor`, создать и поднять
   env-бутылку (п.2).
3. Попытка `bottle create … --kind host` + `start`.

**Ожидаемый**
- env-бутылка работает без администратора; аплаенс найден рядом
  с exe (`appliance/`).
- Host-старт без установленной службы — понятная ошибка с
  подсказкой `bottle-svc --install`; doctor показывает `service` ⚠.

## 8. Автоматическое (CI, к ручному не относится)

- `install-smoke` (по тегу): тихая установка/удаление MSI,
  проверки файлов+службы+CLI; portable-распаковка и отклик —
  все шаги зелёные.
- SHA256SUMS совпадают с загруженными файлами.

---

Приёмка M3 считается пройденной, когда пункты 1–7 выполнены на чистой
VM без дополнительных инструкций, кроме этого чеклиста.
