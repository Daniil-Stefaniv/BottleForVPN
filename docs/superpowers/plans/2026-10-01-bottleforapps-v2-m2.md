# BottleForApps v2 — M2 Implementation Plan (host-бутылки + служба маршрутов + живучесть)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Фирменные GUI-VPN-клиенты работают на хосте параллельно с env-бутылками: привилегированная служба `bottle-svc` держит точечные маршруты endpoint'ов через физический шлюз, демон привязывает RDP-релеи к VPN-адаптерам; плюс живучесть — авто-восстановление env-бутылок после рестарта WSL, идемпотентность к гонкам (поколения в протоколе), параллельный CLI.

**Architecture:** Спека v2 §4.2/§7.2/§9 без изменений. Четвёртый процесс `bottle-svc.exe` (служба Windows, SYSTEM): только add/del/cleanup host-маршрутов `/32`//`128` с валидацией и журналом; маршруты привязаны к сессии демона (named pipe) и убираются при её смерти + сверка при старте службы. Демон получает host-менеджер (endpoints → pin-маршруты → запуск клиента → ожидание интерфейса → релеи с `IP_UNICAST_IF`) и цикл перепроверки DNS (30 с). Живучесть: health-поллинг агента, авто-восстановление env-бутылок, хост-бутылки переводятся в `stopped` при смерти их интерфейса.

**Tech Stack:** Rust (новый крейт `svc`), `windows-service` (служба), `windows-sys` (ACP: CreateIpForwardEntry2 не используем — маршруты через PowerShell `New-NetRoute/Remove-NetRoute -PolicyStore ActiveStore`, прецедент v1), `socket2` (IP_UNICAST_IF/IPV6_UNICAST_IF в релеях демона).

**Spec:** [docs/superpowers/specs/2026-09-30-bottleforapps-v2-design.md](../specs/2026-09-30-bottleforapps-v2-design.md) — §4.2 (bottle-svc), §7.2 (жизненный цикл host-бутылки), §8 (RPC), §9 (таблица надёжности), §10 (безопасность), §11 (двухуровневая установка; MSI остаётся M3).

## Global Constraints

- Всё, что зафиксировано в плане M0+M1, продолжает действовать: Rust stable; `cargo fmt --check` и `clippy -D warnings` зелёные всегда; пользовательские строки — русские; телеметрии нет; секреты не логируются.
- Имена: служба и бинарник `bottle-svc`; канал `\\.\pipe\bottleforapps-routes`; журнал маршрутов `%PROGRAMDATA%\BottleForApps\routes.jsonl` (спека §8).
- bottle-svc принимает ТОЛЬКО `add`/`del`/`cleanup` host-маршрутов `/32`//`128`: loopback/link-local/multicast и любые префиксы длиннее хоста отвергаются; служба не запускает процессы (спека §4.2).
- Маршруты — только `ActiveStore` (исчезают при перезагрузке; спека «Безопасность маршрутов»).
- Таймауты host-бутылки: ожидание интерфейса ≤120 с; перепроверка DNS endpoint'ов — каждые 30 с; новые адреса получают маршруты, старые живут до конца сессии (спека §7.2).
- PROTOCOL_VERSION: 1 → 2 (добавлены instance-поколения; демона и аплаенс обновляем вместе, `bottle upgrade`-подсказка при несовпадении остаётся).
- Проверки на macOS-хосте: unit + `cargo check/clippy --target x86_64-pc-windows-msvc`; поведенческие Windows-тесты — CI `windows-checks` (раннеры под админом — служба регистрируется реально); root/Linux-интеграционные — `agent-integration`.
- Мелочи из M1, вошедшие сюда (из ledger финального ревью): параллельный pipe, Show{runtime-only}, pid-реюз в boot_cleanup, OpenVPN-покрытие CI, мультипир-конфиг — явная ошибка, `validate_target` порт 0, launch с аргументами, мёртвые deps.

## Review Focus

1. **Инъекция в RPC службы** (мусорные адреса, префиксы, v6-заглушки) → валидация до любого syscall; тест — Task 2.
2. **Левые маршруты переживают смерть демона** (крах демона, kill -9 службы, перезагрузка) → session-scoped журналирование + сверка; тесты — Task 5.
3. **Host-бутылка поднялась, но VPN-интерфейс так и не появился / пропал** → `failed` с причиной / `stopped` по живости; тесты — Task 9/11.
4. **Down чужого поколения убивает живую бутылку** (TOCTOU из M1) → instance в протоколе, агент игнорирует чужой Down; тест — Task 1.
5. **Релей привязан не к тому адаптеру** (default route захвачен вторым VPN) → IP_UNICAST_IF устанавливается ДО connect, unittest на порядок вызовов + CI; тест — Task 9.

---

### Task 1: Протокол v2 — instance-поколения

**Files:** Modify: `crates/core/src/protocol.rs`, `crates/agent/src/bottles.rs`, `crates/agent/src/control.rs`, `crates/daemon/src/manager.rs`
**Interfaces:**
- Produces: `PROTOCOL_VERSION = 2`; `UpArgs { …, pub instance: u64 }`; `AgentRequest::Down { id: String, instance: u64 }`; агент хранит instance в NetnsHandle/Bottles, `Down` с чужим instance → `AgentReply::Error{code: STALE_INSTANCE, …}` (новая константа `err::STALE_INSTANCE`), НЕ трогает бутылку; демона manager передаёт свой generation (уже есть из M1) в Up/Down.
- Совместимость: версия 2 несовместима с аплаенсом v1 — ошибка VERSION с текстом про `bottle upgrade` уже реализована (T15), не менять её.

**Steps:** [t] RED: codec round-trip Up с instance; агент: Down чужого instance → STALE_INSTANCE и бутылка жива (unit через handle_request + Bottles с моком? — интеграционно через existing fake-agent паттерн daemon-тестов); daemon: Up/Down несут generation. → GREEN → fmt/clippy/test + musl-check → commit `feat(core,agent,daemon): instance generations in protocol v2`.

### Task 2: bottle-core::svcrpc — протокол службы маршрутов

**Files:** Create: `crates/core/src/svcrpc.rs`
**Interfaces:**
- `pub const SVC_PIPE: &str = r"\\.\pipe\bottleforapps-routes";`
- `pub enum SvcRequest { Hello { daemon_pid: u32 }, Add { dest: IpAddr, gateway: IpAddr, iface_alias: String }, Del { dest: IpAddr }, Cleanup, List }`
- `pub enum SvcReply { Ok, RouteList { routes: Vec<RouteEntry> }, Error { code: SvcErrCode, message: String } }`; `pub struct RouteEntry { pub dest: IpAddr, pub gateway: IpAddr, pub iface_alias: String, pub session: u64 }`
- `pub fn validate_route(dest: IpAddr) -> Result<(), SvcError>`: только одиночные адреса (тип IpAddr сам исключает префиксы), loopback/link-local/multicast → отказ; тесты: 8.8.8.8 ok, 203.0.113.10 ok, 127.0.0.1/::1/169.254.0.1/224.0.0.1/ff02::1 → отказ (Review Focus #1).
- Транспорт — существующий `LengthJsonCodec`.

**Steps:** RED round-trip всех вариантов + валидация → GREEN → commit `feat(core): svc route rpc types + validation`.

### Task 3:svc — каркас службы Windows

**Files:** Create: `crates/src…/` — крейт `crates/svc`, binary `bottle-svc` (депы: `windows-service`, `bottle-core`, `tokio`, `tracing`, `tracing-subscriber`)
**Interfaces:**
- `fn main()`: аргументы `run` (по умолчанию), `--install`, `--uninstall`, `--console` (отладка без SCM). `--install/--uninstall` требуют админа (проверка + русский отказ) и работают через `windows_service::service_dispatcher` + `sc`-семантику crate'а.
- `service_main` → `ServiceState::Running` → внутренний runtime: запуск pipe-сервера (Task 6 подсоединит); остановка по SCM-сигналу → graceful.
- Реестр службы: имя `BottleForAppsRouteHelper`, автозапуск, описание русское (спека §11 наследует имя из v1-спеки).

**Steps:** каркас компилируется на msvc-target; unit: parse аргументов; CI-поведенческий тест (install→query→uninstall) помечен `#[cfg(windows)] #[ignore]`, исполняет Task 6-джоба → commit `feat(svc): windows service skeleton`.

### Task 4: svc — операции над маршрутами

**Files:** Create: `crates/svc/src/routes.rs`
**Interfaces:**
- `#[cfg(windows)] pub fn add_route(dest, gateway, iface_alias) -> Result<(), SvcError>` / `del_route(dest)` / `list_matching(dest_prefix_family)`: PowerShell `New-NetRoute -DestinationPrefix <ip>/32|/128 -NextHop <gw> -InterfaceAlias <a> -PolicyStore ActiveStore -ErrorAction Stop` (+ `Remove-NetRoute`/`Get-NetRoute`), stdout/stderr захват, ненулевой выход → ошибка с stderr.
- `pub fn route_ps_args(...) -> Vec<String>` — чистый конструктор аргументов (unit-тестируемый на любой ОС): /32 для v4, /128 для v6, кавычки не нужны (аргуты массивом).
- `#[cfg(not(windows))]` стабы → Err("только Windows").
- Поведенческие тесты `#[cfg(windows)]`: add → `Get-NetRoute` видит → del → нет (раннер под админом; реальный ActiveStore).

**Steps:** RED unit конструктора → GREEN → CI-тесты в коде → commit `feat(svc): active-store route operations`.

### Task 5: svc — журнал, сессии, сверка

**Files:** Create: `crates/svc/src/journal.rs`, `crates/svc/src/sessions.rs`
**Interfaces:**
- `SessionId = u64`; сессия создаётся на pipe-подключение демона (`Hello{daemon_pid}`), живёт пока соединение открыто; все Add/Del журналируются в `routes.jsonl` (append; строка {op, dest, session, ts}) и в map в памяти.
- При закрытии соединения демона → `del_route` всех его маршрутов + журнал (демон упал — маршруты не переживают; спека §9).
- `pub fn reconcile()` при старте службы: читает журнал, для каждого уникального dest сверяет `Get-NetRoute` (ActiveStore): маршрут есть, а сессий нет → удалить + записать op. Идемпотентен на пустом состоянии.
- Unit: журнал как чистая редукция (ops → live-set) на фейковых командах; поведение reconcile на windows-CI.

**Steps:** RED unit live-set (add/add-other-session/del/крах-сессии) → GREEN → commit `feat(svc): session-scoped route journal + reconcile`.

### Task 6: svc — pipe-сервер + интеграция службы

**Files:** Create: `crates/svc/src/pipe.rs`; Modify: `crates/svc/src/main.rs` (пуск сервера в service_main)
**Interfaces:**
- named pipe `SVC_PIPE`, `LengthJsonCodec<SvcRequest/SvcReply>`; несколько клиентов последовательно; фоновый поток на соединение НЕ нужен — демон один.
- `Hello` от нового соединения: предыдущая сессия демона (если была) закрывается по таймаутуpipe-поллинга соединения (демон перезапустился).
- CI `windows-checks` дополняется шагом: `cargo test -p svc -- --ignored` (install службы, RPC add/list/del, kill демона-клиента → маршруты убраны, uninstall).

**Steps:** цикл сервера + транспорт-тесты (как T14 daemon) → CI-шаг → commit `feat(svc): route pipe server + ci service tests`.

### Task 7: daemon — SvcClient

**Files:** Create: `crates/daemon/src/svc_client.rs`
**Interfaces:**
- `pub struct SvcClient { … }`: `connect() -> Result<Self, DaemonError>` (pipe, Hello, хранит соединение); `add(dest, gw, iface)`, `del(dest)`, `cleanup()`; ошибки службы → DaemonError с её сообщением.
- `pub fn svc_available() -> bool` (быстрая проверка pipe без блокировки).
- Host-старт без службы → `CliReply::Error{"Служба маршрутов BottleForApps не установлена — host-бутылки недоступны (bottle-svc --install от администратора)"}` (спека §2: без UAC-fallback).
- Тесты: фейк-служба (in-process, LengthJsonCodec) — Hello/Add/Del/ошибки; unavailable-путь.

**Steps:** RED на фейке → GREEN → commit `feat(daemon): svc client`.

### Task 8: daemon — endpoints + физический шлюз

**Files:** Create: `crates/daemon/src/host_net.rs`
**Interfaces:**
- `pub fn resolve_endpoints(hosts: &[String]) -> Vec<IpAddr>`: std `ToSocketAddrs` (порт-заглушка :53 не нужна — `(host, 0).to_socket_addrs()`); дедуп v4+v6; пустой результат → ошибка «endpoint не резолвится».
- `pub fn physical_gateway(exclude_idx: &[u32]) -> Result<(IpAddr, String), DaemonError>`: `Get-NetRoute -DestinationPrefix 0.0.0.0/0 | Sort InterfaceMetric` (PS), первый чей InterfaceIndex не в exclude и InterfaceAlias не vEthernet (WSL…) и не подошедший VPN-адаптер host-бутылок; v6-вариант при наличии v6-endpoint.
- Unit: parse вывода PS на фикс-файксах (multi-строки, метрики, фильтры); поведенческие — windows-CI.

**Steps:** RED парсеры → GREEN → commit `feat(daemon): endpoint resolution + physical gateway detection`.

### Task 9: daemon — host-менеджер: старт, интерфейс, релеи

**Files:** Create: `crates/daemon/src/host_manager.rs`; Modify: `manager.rs` (маршрутизация kind=Host → host_manager)
**Interfaces:**
- Старт по спеке §7.2: resolve endpoints → svc.add для КАЖДОГО активного host-endpoint (вкл. чужие бутылки: менеджер держит множество всех host-endpoints активных сессий) → `Command::new(client).args(split(args))` (простой split по пробелам, кавычки — shlex-минимум) → снапшот адаптеров (`Get-NetAdapter`) → поллинг ≤120 с (шаг 2 с): явно заданный `interface` или единственный новый `Up` (несколько новых → failed «укажите interface») → релеи: listener 127.0.0.1, исходящий сокет через `socket2` с `IP_UNICAST_IF`/`IPV6_UNICAST_IF` ДО connect (Review Focus #5) → `up`.
- `expose.launch` у host-бутылок тоже работает; `socks5` — SOCKS5 с тем же биндингом.
- Юнит-тесты на macOS: машина состояний с фейками (svc-клиент, адаптер-поллер, спавнер — трейты); порядок вызовов биндинга зафиксирован тестом на socket2-обёртке `bind_to_iface(sock, idx)` (unit: корректный порядок set→connect через мок? — чистая функция параметров).
- CI windows-поведенческий: фейковый «VPN-клиент» = powershell-скрипт, создающий loopback-псевдоинтерфейс? — НЕТ: используйте существующий WSL vEthernet как «появившийся» интерфейс недоступно… принимаем: поведенческий e2e host-бутылки — ручной чеклист (Task 16); CI покрывает unit+интеграцию с фейками.

**Steps:** RED сценариев на фейках (успех; нет интерфейса → failed 120с сжат до 2с в тесте; несколько новых интерфейсов → failed; svc недоступна → ошибка установки) → GREEN → commit `feat(daemon): host bottle manager`.

### Task 10: daemon — DNS-цикл 30 с и stop host-бутылки

**Files:** Modify: `crates/daemon/src/host_manager.rs`, `manager.rs`
**Interfaces:**
- Фоновая задача на daemon-цикле: каждые 30 с re-resolve активных host-endpoints; новые адреса → svc.add; прежние остаются в живом множестве до stop сессии (спека §7.2); ошибки резолва — warn, не fatal.
- `stop`: релеи вниз, svc.del всех адресов сессии (вкл. накопленные), клиент НЕ закрывается (спека: нет API; warning-строка в ответе CLI).
- Юнит: таймлайн-тест с фейковым резолвером (t=0 A, t=60 A+B → добавлен B, A жив) + stop убирает оба.

**Steps:** RED → GREEN → commit `feat(daemon): host dns re-check loop + stop`.

### Task 11: daemon — живучесть и авто-восстановление

**Files:** Modify: `crates/daemon/src/manager.rs`, `crates/daemon/src/main.rs`
**Interfaces:**
- Health-цикл (10 с): `agent.alive()`; неудача → все env-бутылки `Degraded`, попытка `ensure_appliance` с бэкоффом (5с/15с/60с, ≤∞); успех → повторный Up всех env-бутылок (порты те же — пересервировать; instance-поколения защищают от двойных Up), состояние `up`; host-бутылки: поллинг их интерфейса (Get-NetAdapter), умер → `stopped` (рефайн спеки §9: проверка живости вместо слепого stop — фиксируется рулингом в ledger).
- Смерть relay-задачи (JoinSet завершился с ошибкой) → бутылка `Degraded{причина}`.
- Sleep/resume покрывается тем же health-циклом (спека §9).
- Юнит: фейк-агент умирает/восстаёт → таймлайн состояний (Degraded → up; порты возвращены); host-интерфейс исчез → stopped.

**Steps:** RED таймлайнов → GREEN → commit `feat(daemon): resilience loop + auto-restore`.

### Task 12: agent — pid-реюз и жизнестойкость boot_cleanup

**Files:** Modify: `crates/agent/src/bottles.rs`
**Interfaces:**
- Перед сигналом из pid-файла: читать `/proc/<pid>/cmdline`, убивать ТОЛЬКО если содержит `bottle-agent` (и `relay`/маркер туннеля) — защита от реюза pid после рестарта агента без reboot VM (ledger T11).
- Чистая фн `should_kill_pid(cmdline: &str) -> bool` + unit; интеграционно — существующий bottles_e2e (он уже гоняет boot_cleanup на живом стенде).

**Steps:** RED unit → GREEN → musl-check → commit `fix(agent): pid-reuse guard in boot cleanup`.

### Task 13: daemon pipe — параллельные клиенты; CLI-мелочи

**Files:** Modify: `crates/daemon/src/pipe.rs`, `crates/cli/src/main.rs`, `crates/cli/src/render.rs`, `crates/core/src/profile.rs`, `crates/daemon/src/manager.rs`
**Interfaces:**
- Pipe-сервер: несколько экземпляров канала + `tokio::spawn` на соединение (последовательность ответов не страдает — состояние за мьютексами).
- CLI: `Show{profile: None, runtime: Some}` рендерит runtime-блок + exit 1 (не «не найдена» без деталей).
- `validate_target`: порт 0 → InvalidTarget (синхрон с TargetAddr).
- `launch`: сплит аргументов (мини-shlex: кавычки), используется и host- и env-бутылками.
- Мёртвые deps: убрать `serde` из agent-манифеста если не используется, `toml`/`serde_json` из daemon, `serde_json` из cli (проверить grep'ом перед удалением).

**Steps:** RED на каждый пункт (pipe: два параллельных клиента оба получают ответы ≤ таймаута) → GREEN → commit `feat(daemon,cli): parallel pipe + m1 leftovers`.

### Task 14: OpenVPN-интеграционный тест в CI

**Files:** Create: `crates/agent/tests/openvpn_e2e.rs`; Modify: `.github/workflows/ci.yml`
**Interfaces:**
- Стенд как wg_handshake (два netns, veth 253-подсеть, host-конец .2 в default ns): `openvpn --genkey secret` в тесте; srv-netns: `openvpn --dev tun0 --proto udp --port 1194 --ifconfig 10.99.1.1 10.99.1.2 --secret key`; бутылка: Bottles::up(protocol=OpenVpn, config со статическим ключом); критерий: tun0 в ns появился ≤60 с + SOCKS5 negotiate через мост (echo-сквозной welcome, как в bottles_e2e).
- CI: `apt-get install openvpn` добавить в agent-integration (уже ставит iproute2/wg…).

**Steps:** тест пишется (CI исполнит; локально musl-check) → commit `test(agent): openvpn e2e + ci apt`.

### Task 15: валидатор конфигов и мелочи профиля

**Files:** Modify: `crates/agent/src/wg.rs`, `crates/daemon/src/manager.rs`
**Interfaces:**
- wg-парсер: конфиг с БОЛЕЕ одним `[Peer]` → явная `InvalidConfig` «поддерживается один пир» (сейчас — невнятный wg setconf-сбой; ledger T10).
- `read_config` (daemon): имя файла из профиля валидируется `^[A-Za-z0-9._-]+$` (без traversal `../`; ledger-минор T16).

**Steps:** RED оба → GREEN → commit `fix(agent,daemon): config validation hardening`.

### Task 16: Приёмка M2 — чеклист, README, doctor-минимум

**Files:** Create: `docs/checklists/m2-verification.md`; Modify: `README.md`
**Interfaces:**
- Чеклист (спека §14 M2: «destructive-набор проходит; host-бутылка с реальным клиентом работает параллельно с env»): (1) host-бутылка с реальным AnyConnect/FortiClient + env-бутылка WG одновременно; (2) endpoint'ы обоих через физику (`Get-NetRoute` / трассировка); (3) kill -9 демона → маршруты исчезли ≤5 с; (4) kill -9 службы → журнал-сверка при её старте; (5) `wsl --shutdown` → env авто-восстановились, host жив/остановлена по интерфейсу; (6) двойной start/stop-шторм; (7) параллельные CLI-команды во время длинного start.
- README: раздел «Host-бутылки (фирменные клиенты)» — установка службы, формат host-профиля, ограничения (kill switch вендоров), обновление статусов таблицы (M2 ✅).

**Steps:** документы написаны, согласованы с фактическим поведением (сверка каждого пункта с кодом) → commit `docs: m2 acceptance checklist + readme host bottles`.

## Что НЕ входит в этот план

- MSI/portable-установщики, `bottle doctor`, файловые журналы + `logs export`, команда `create` — **M3**.
- GUI (Tauri), macOS — **M4**.
- Замена PowerShell-маршрутов на Win32 API (CreateIpForwardEntry2) — осознанная отсрочка: операции редки (старт/стоп бутылки), PS-путь проверен в v1; кандидату в M3-оптимизацию.
- Автоотключение фирменных клиентов, UDP — вне планов (спека-не-цели).
