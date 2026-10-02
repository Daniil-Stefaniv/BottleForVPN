# BottleForApps v2 — M3 Implementation Plan (установщики, наблюдаемость, create/upgrade/doctor)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Продукт ставится на чистую машину без ручных шаманств: MSI (один UAC: служба + WSL + файлы) и portable-zip (без админа, env-only); наблюдаемость — файловые JSONL-журналы + `logs export`; UX-команды `create/delete/upgrade/routes cleanup/doctor`; кодовый M3-бэклог из рулингов M1/M2.

**Architecture:** Спека v2 §11 (два уровня установки), §«Диагностические журналы», §14 (критерий M3). Новых подсистем нет: daemon получает файловый лог и новые IPC-операции; svc — полную валидацию шлюза; installer/ обрастает WiX-проектом и portable-скриптом; CI — релизной джобой.

**Tech Stack:** Rust (существующие крейты; `tracing-appender` для ротации), WiX Toolset v4/v5 (MSI, собирается в CI через choco), PowerShell (installer-скрипты portable), zip.

**Spec:** [docs/superpowers/specs/2026-09-30-bottleforapps-v2-design.md](../specs/2026-09-30-bottleforapps-v2-design.md) — §2 (не-цели: без UAC-fallback), §4.4 (CLI), §11 (установка), §«Диагностические журналы», §14 (M3).

## Global Constraints

- Всё из M1/M2 действует: Rust stable; fmt + clippy -D warnings всегда; русские пользовательские строки; телеметрии нет.
- Журналы: JSONL, каталог `<data>/logs` (daemon) и `%PROGRAMDATA%\BottleForApps\logs` (svc — SYSTEM); ротация ежедневно, хранение 30 дней (спека «Диагностические журналы»); секреты (токен, конфиги, ключи, аргументы клиентов) не логируются — маскирование (спека §10).
- `logs export`: ZIP локально; предупреждение о содержимом (IP/имена/endpoint) перед отправкой — текст из спеки.
- MSI: один UAC; ставит bottle/bottled/bottle-svc + службу (`bottle-svc --install`) + включает WSL (`wsl --install --no-distribution`, перезагрузка обрабатывается: BuyMessage/after-reboot-доинка — WiX-механика на исполнителя) + rootfs в `ProgramFiles\BottleForApps\appliance` + PATH; версия MSI = `[workspace.package].version`.
- Portable: zip без прав админа; только env-бутылки; rootfs рядом с exe; host-бутылки — понятная ошибка (уже есть).
- Разрешение rootfs демона: сначала `<data>/appliance/…`, затем `<exe_dir>/appliance/…` (покрывает MSI и portable; новое правило, рулинг).
- `upgrade` не теряет профили: unregister только дистрибуции WSL; токен регенерируется; профили в `<data>` не трогаются.
- Проверки: локально macOS (unit + msvc/musl-чек); Windows-поведенческое — CI; WiX-сборка — CI (windows + choco wixtoolset); ручная приёмка — чеклист.

## Review Focus

1. **Секреты в новых логах/экспорте** — конфиги VPN, токен, ключи, аргументы клиентов не должны попасть ни в JSONL, ни в ZIP; тест-дубль §10. Тест — Task 1/2.
2. **upgrade уничтожает рабочий сетап** (unregister во время живых бутылок; потеря токена; несовместимый новый rootfs) — защита: отказ при живых бутылках, версионный handshake после, пере-провижининг токена. Тест — Task 5.
3. **MSI поверх установленной версии / даунгрейд** — major upgrade корректно сменяет службу и файлы; даунгрейд отвергается. Тест — CI Task 11.
4. **doctor ложно-красный на чистой машине** (WSL включён но дистрибуции нет; служба не установлена — это норма для portable) — статусы «ok/warn/fail» с русскими пояснениями, а не бинарь. Тест — Task 3.
5. **create перезатирает существующий id** (профиль+конфиг пользователя) — отказ без `--force`. Тест — Task 4.

---

### Task 1: Файловые журналы демона и службы

**Files:** Modify: `crates/daemon/src/main.rs`, `crates/svc/src/main.rs`; Create: `crates/daemon/src/logging.rs`, `crates/svc/src/logging.rs`
**Interfaces:**
- Общий паттерн (дублирование крейтов — прецедент hex/ps_value): `pub fn init_file_logging(dir: &Path, base: &str) -> Result<(), …>`: tracing_subscriber + `tracing-appender` (rolling::daily, prefix `<base>.`, json-формат), retention 30 дней (фоновая задача удаления при старте: файлы старше 30d — просто удаление по mtime, не парсинг), stdout-слой сохраняется (svc — в SCM-лог? нет: svc пишет только файл + stderr).
- daemon: `<data>/logs`; svc: `%PROGRAMDATA%\BottleForApps\logs` (создать каталог; не-windows — tempdir для тестов).
- Аудит маскирования: grep-подтверждение, что ни один новый форматтер не выводит `UpArgs.config`/token/ключи; unit-тест на хелпер `mask_args(args: &str) -> String` (маскировать значения после `--`-подобных флагов? НЕТ — проще: `mask_args` для vpn-args клиентов: заменить всю строку на `<аргументы скрыты>`; тест).
- Тесты: init в tempdir → файл появился с сегодняшним префиксом; retention удаляет старый (создать файл с mtime −31d → удалён, −29d → жив).

### Task 2: `bottle logs export`

**Files:** Create: `crates/daemon/src/export.rs`; Modify: `crates/core/src/ipc.rs` (новый CliRequest::LogsExport{dest: Option<PathBuf>} / CliReply::Path(String)|Error), daemon pipe, CLI
**Interfaces:**
- Демон собирает ZIP: `<data>/logs/*` + svc-логи `%PROGRAMDATA%\BottleForApps\logs/*` (если есть) + `system.txt` (версии демона/агента/аплаенса из HelloAck, PROTOCOL_VERSION, состояние бутылок, профили БЕЗ конфигов).
- dest: путь пользователя или `<data>/bottleforapps-logs-<YYYYMMDD-HHMM>.zip`; CLI печатает путь + предупреждение из спеки (IP/имена интерфейсов/endpoint — просмотрите перед отправкой).
- ZIP — dep `zip` (daemon). Тесты: экспорт в tempdir → zip читается, содержит лог-файл и system.txt; без svc-логов — не падает; предупреждение содержит «просмотрите».

### Task 3: `bottle doctor`

**Files:** Create: `crates/daemon/src/doctor.rs`; Modify: ipc (CliRequest::Doctor / CliReply::DoctorReport{items: Vec<CheckItem{code, status, detail}>}), CLI render
**Interfaces:**
- Проверки (каждая: ok|warn|fail + русское пояснение): `wsl` (wsl --status / -l -q: есть ли дистрибуции; выключен → fail с командой включения; включён без дистрибуций → warn «нормально для portable»); `appliance` (импортирована? версия из HelloAck == PROTOCOL_VERSION? несовместимо → подсказка upgrade); `service` (svc_pipe_exists: установлена → ok «host-бутылки доступны»; нет → warn «env-only, bottle-svc --install»); `agent` (alive()); `profiles` (store.list: каждую строку — ок/ошибка парсинга); `ports` (занятость зарезервированных фиксированных портов профилей — bind-проба).
- CLI: таблица `✓/⚠/✗  код  пояснение`; exit 0 если нет fail (warn допустим).
- Тесты на фейках (trait-инъекция wsl/agent/svc, как в appliance.rs): каждая проверка в трёх состояниях; render содержит эмодзи-статусы; exit-коды.

### Task 4: `bottle create` и `bottle delete`

**Files:** Modify: `crates/core/src/ipc.rs`, daemon pipe/manager, CLI (clap-подкоманды)
**Interfaces:**
- IPC: `CliRequest::Create{profile: BottleProfile, config_source: Option<PathBuf>}` → daemon: validate_id (уже есть) + отказ если id занят (Review Focus #5: без --force-перезаписи; `force: bool` в запросе, CLI-флаг `--force`); store.save (копирует конфиг — существующая семантика); reply Created{id}. `Delete{id}` → отказ если бутылка активна; удалить toml+conf; reply Deleted{id} (+warning если конфиг-файл отсутствовал).
- CLI: `bottle create <id> --name --kind env|host --protocol wireguard|openvpn --config <путь> [--vpn <exe> --vpn-args --endpoint <host>…|--vpn-interface] [--socks5 N] [--forward target]… [--dns IP] [--launch cmd]` — собирает профиль, печатает итоговый TOML и пути; `bottle delete <id>`.
- Валидация host-полей: endpoints обязателен для kind=host (иначе ошибка создания), client тоже; для env — config.
- Тесты: create env (конфиг скопирован, файл создан); create занятый id → отказ; create --force → перезапись; create host без endpoints → ошибка; delete активной → отказ; delete снятой → файлы удалены; CLI-парсер всех флагов.

### Task 5: `bottle upgrade` + `routes cleanup` + no-op stop

**Files:** Modify: ipc, daemon (appliance.rs, manager.rs), CLI
**Interfaces:**
- `CliRequest::Upgrade` → демон: если есть активные бутылки → Error «остановите бутылки перед обновлением» (Review Focus #2); `wsl --unregister bottleforapps`; удалить `appliance-token` (иначе «токен утерян»-ошибка при следующем старте — устоявшийся текст); повторный ensure_appliance из `--rootfs`-пути; HelloAck-проверка версии; reply: старая/новая appliance_version.
- `CliRequest::RoutesCleanup` → демона → svc.cleanup() (если svc нет — Ok с warning «служба не установлена»); reply Done{warning}.
- no-op stop: повторный stop уже остановленной → `Stopped{id, warning: Some("не была запущена")}` вместо Error (спека §5.2; рулинг M3-бэклога).
- Тесты: upgrade при живой бутылке → отказ; upgrade на фейке → unregister+токен удалён+новая версия в reply; token-regen путь (appliance-token отсутствует после upgrade); routes cleanup без службы → warning; double-stop → Stopped с warning.

### Task 6: Кодовый M3-бэклог (батч)

**Files:** Modify: `crates/svc/src/routes.rs`, `crates/agent/src/bottles.rs`, `crates/daemon/src/manager.rs`, `crates/daemon/src/ps_query.rs`, `crates/daemon/src/host_manager.rs`
**Interfaces (каждый пункт — рулинг из ledger M1/M2):**
- `run_ps` сигнатура сужается: принимает `RouteCmd`+параметры (не сырые аргументы) — единственная точка `route_ps_args`; тесты-компиляция.
- Agent `provision` TOCTOU: вставка в реестр только если слот всё ещё принадлежит этой операции (перепроверка под локом после провижининга; чужое поколение → разбор своего и BadInstance-ошибка) — тест на гонку в агентных unit-паттернах.
- Health-цикл: если активных env-бутылок нет — пробу агента пропускаем (VM может спать; T11-концерн «wsl.exe каждые 10 с вечно») — пин-тест таймлайна.
- `ps_query`: чтение stdout/stderr параллельно (spawn + threads/tokio copy до wait — дедлок >64КБ устранён) — unit с большим выводом через `Write-Output`-генератор? не-Windows стаб не исполняет — тест windows-CI + логика-тест на буферизацию хелпера.
- Инвариант последовательности control-соединений агента — комментарий в control.rs (документация, не код).
- restore_after_recovery vs user-stop: флаг «пользовательская остановка» на период восстановления (stop между stop и start восстановления не перезапускается) — таймлайн-тест.

### Task 7: Полная валидация шлюза службой (спека §4.2 дословно)

**Files:** Modify: `crates/svc/src/pipe.rs`, `crates/svc/src/routes.rs`; Create: helper в routes.rs
**Interfaces:**
- dispatch(Add): после validate_gateway — сверка `gateway == текущий next-hop интерфейса iface_alias` (Get-NetRoute default-prefix семейства шлюза → фильтр по InterfaceAlias): не совпал → Error{VALIDATION, «шлюз не соответствует интерфейсу»}; интерфейс без default-маршрута → та же ошибка.
- Дубликат Add с ДРУГИМИ параметрами (gateway/iface): сейчас молча держит старый OS-маршрут — теперь: сверка обнаружит расхождение → VALIDATION-отказ (последовательное поведение, закрывает M2-минор).
- Кэш таблицы шлюзов на ~5 с (анти-шторм PS при старте нескольких бутылок).
- Тесты: unit на чистом `check_gateway_matches(gw, alias, table)` (фикстуры); pipe-диспетчер на фейке PS-раннера; CI-поведенческий svc_win: легитимный Add со шлюзом реального default → Ok.

### Task 8: Portable-сборка

**Files:** Create: `scripts/build-portable.ps1` (или .sh — решайте: целевая ОС Windows → ps1, но CI может звать pwsh; ОПИСАНИЕ в appliance/README); Modify: `crates/daemon/src/main.rs` (разрешение rootfs `<exe_dir>/appliance/` — второй приоритет после `<data>/appliance/`)
**Interfaces:**
- Скрипт (CI windows): cargo build --release (daemon+cli+svc, msvc host) → каталог `dist/portable/BottleForApps/`: bottle.exe, bottled.exe, bottle-svc.exe (для --install по желанию админа), appliance/bottleforapps-rootfs.tar.gz (из artifacts), INSTALL.txt (русский: portable = env-only; для host — bottle-svc --install от админа), → zip `artifacts/BottleForApps-portable-<version>-win-x64.zip` + SHA256.
- Daemon rootfs-резолюция: data → exe_dir (рулинг Global Constraints); тест резолвера (чистая функция над путями).
- Локально на macOS: скрипт не исполняется — синтаксис/структура; CI-джоба Task 11 прогонит.

### Task 9: WiX MSI

**Files:** Create: `installer/msi/BottleForApps.wxs` (+ вспомогательные .wxs/RG), `installer/msi/build-msi.ps1`; Modify: CI
**Interfaces:**
- Компоненты: ProgramFiles\BottleForApps\{bottle.exe, bottled.exe, bottle-svc.exe, appliance\bottleforapps-rootfs.tar.gz}; служба: `bottle-svc --install` через custom action (или ServiceInstall WiX на exe? — служба ставится нашим --install с русским описанием; custom action от админа, after InstallFiles, rollback-чистка); WSL: custom action `wsl --install --no-distribution` (exit 3010/reboot → ScheduleReboot); PATH (Environment element,_machine);_major upgrade_ (UpgradeCode фиксирован, AllowDowngrades=no); версия из workspace (передать -arch x64, -dProductVersion=…).
- UI минимальный (WixUI_InstallDir необязательно — по умолчанию без UI, тихая установка допустима).
- Сборка: `build-msi.ps1` — wix build (v4+ CLI) → `artifacts/BottleForApps-<version>-win-x64.msi` + SHA256.
- CI: windows-раннер + `choco install wixtoolset` (или dotnet tool install wix) — исследовать доступность; MSI-ICE-валидация (`wix build` сам валидирует).
- Локально macOS: только структура файлов и код-ревью; CI собирает.

### Task 10: Релизная CI-джоба

**Files:** Modify: `.github/workflows/ci.yml` (+ `release` workflow или job в ci по тегу)
**Interfaces:**
- Job `release-build` (windows, по тегу v*): checkout → rust → build rootfs (docker на windows-раннере ЕСТЬ? windows docker = linux-контейнеры требует Hyper-V — НА РАННЕРАХ windows docker доступен как linux-containers… нестабильно; АЛЬТЕРНАТИВА: rootfs собирает существующая appliance-build (ubuntu) — needs: appliance-build + download-artifact) → cargo build --release → portable-скрипт → msi-скрипт → все артефакты + SHA256SUMS.txt → upload; по тегу — GitHub Release (softprops/action-gh-release) c текстом релиза на русском.
- Job `install-smoke` (windows): MSI из артефакта → msiexec /i /qn → проверка: ProgramFiles файлы, служба установлена (sc query), `bottle --version`, `bottle doctor` (ожидание: wsl ok/warn, appliance ok после первого старта демона? — демон не запускаем как сервис; smoke ограничен файлами+службой+CLI) → msiexec /x uninstall → чистота. Portable-smoke: распаковка zip → bottle --version → doctor (portable-ветка, без службы).
- Версия: тег должен совпадать с `[workspace.package].version` (шаг-проверка, несовпадение — fail).

### Task 11: Документация под релиз

**Files:** Modify: `README.md`, `appliance/README.md`; Create: `docs/checklists/m3-verification.md`
**Interfaces:**
- README: установка через MSI/portable как PRIMARY-путь (сборка из исходников — секция «для разработчиков»); команды create/delete/upgrade/doctor/logs export/routes cleanup в справочник; статус-таблица M3 ✅; ограничения обновить (автозапуск демона — ярлык? MSI кладёт ярлык в Startup? РЕШЕНИЕ: MSI — опция ярлыка автозапуска HKCU Run по умолчанию ВЫКЛ; документируем ручной ярлык — как сейчас).
- Чеклист M3 (спека §14: «чистая машина: установка → первая бутылка без ручных шаманств»): чистая Win-VM: (1) MSI → перезагрузка (если попросил) → bottle doctor → все проверки зелёные/warn-free кроме сервис-зависимых; (2) bottle create (env WG) → start → RDP/SSH через проброс; (3) host-бутылка: служба уже от MSI → create host → старт с реальным клиентом; (4) logs export → ZIP валиден, секретов нет (визуально); (5) upgrade на битый аплаенс (подменить rootfs старой версией) → handshake-ошибка → upgrade → работает; (6) uninstall MSI → служба/файлы/PATH чисты; portable: (7) zip без админа → env-бутылка работает, host → предупреждение.
- Каждый пункт: Шаги / Ожидаемый / Где смотреть.

### Task 12: Финальная сводка версий и релизный прогон (локальная часть)

**Files:** Modify: `Cargo.toml` ([workspace.package] version → 2.0.0-rc1? РЕШЕНИЕ: 0.2.0 — до релиза), README-бейджи-плейсхолдеры
**Interfaces:** версия одна во всех местах (MSI, zip-имена, --version, appliance-версия = CARGO_PKG_VERSION агента); README-согласованность; локальные гейты зелёные; git tag НЕ ставим (релиз — после CI и ручной приёмки).

## Что НЕ входит

- GUI (Tauri), macOS — M4.
- Автообновление самого приложения, подпись кода сертификатом (осознанно: требует покупки; MSI не подписан — SmartScreen предупреждает; задокументировать в README-релизе; каркас: шаг подписи в release-workflow закомментирован).
- Централизованный сбор логов/телеметрия — никогда (спека).
- Мультидемон (несколько служб/сессий) — вне модели.
