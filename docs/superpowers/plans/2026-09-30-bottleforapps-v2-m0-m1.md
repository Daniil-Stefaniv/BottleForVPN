# BottleForApps v2 — M0+M1 Implementation Plan (env-бутылка, вертикальный срез)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** С нуля построить Rust-workspace и довести env-бутылку до конца: профиль TOML → демон `bottled` → апплайенс WSL2 → `bottle-agent` создаёт network namespace с WireGuard-туннелем → SOCKS5 и TCP-пробросы на `127.0.0.1`, трафик выходит через физическую сеть хоста.

**Architecture:** Три процесса: `bottled` (демон в сессии пользователя, владелец профилей/портов/слушателей `127.0.0.1`), `bottle-agent` (root-исполнитель внутри WSL2-дистрибуции `bottleforapps`, создаёт/разрушает netns, stateless), `bottle` (тонкий CLI к демону через named pipe). Изоляция — network namespace на бутылку; туннели userspace (`wireguard-go` через `/dev/net/tun`), без драйверов. Все решения ниже аргументируются спекой, номера секций указаны по ходу.

**Tech Stack:** Rust (stable, workspace), tokio, serde/serde_json/toml, thiserror, tracing (JSON), clap; Alpine rootfs (`wireguard-go`, `wireguard-tools`, `openvpn`, `iproute2`, `iptables`), `wsl.exe --import`; CI GitHub Actions (ubuntu + windows).

**Spec:** [docs/superpowers/specs/2026-09-30-bottleforapps-v2-design.md](../specs/2026-09-30-bottleforapps-v2-design.md)

## Global Constraints

- Rust stable, закреплён в `rust-toolchain.toml`; `cargo fmt --check` и `clippy -D warnings` зелёные всегда (спека §12).
- Пользовательские строки (CLI, ошибки) — на русском; идентификаторы кода — английские (спека §4.4).
- Телеметрии нет; секреты (конфиги WG/OpenVPN, токен) никогда не пишутся в журналы (спека §10).
- Протокол демона и агента: length-prefixed JSON (u32 BE + JSON), `PROTOCOL_VERSION = 1`; несовпадение версий — блокирующая ошибка (спека §8).
- Имена: дистрибуция WSL — `bottleforapps`; control-порт агента — `0.0.0.0:7777` в VM; named pipe демона — `\\.\pipe\bottled`; каталог данных — `%LOCALAPPDATA%\BottleForApps` (в тестах/разработке переопределяется флагом `bottled --data-dir`, файлы: `bottles/<id>.toml`, `appliance-token`, `logs/`) (спека §5.1, §8).
- Автоподбор портов — из 49152..=65535, порт закрепляется за сессией (спека §5.1).
- Таймауты: handshake WireGuard ≤30 с, OpenVPN ≤60 с (спека §7.1).
- Внутрибутылочная адресация: подсеть `10.77.<idx>.0/30` на бутылку, `idx ∈ 1..=250`; veth-пара `p<idx>`/`b<idx>` (имена ≤15 символов) (спека §6).
- Апплайенс stateless: конфиги живут в tmpfs VM только на время сессии (спека §4.3).
- Идентификатор бутылки валидируется `^[a-z0-9][a-z0-9-]*$` до любого использования в путях/командах (спека §5.1 + безопасность §10).
- Этап M1 = только env-бутылки и `kind="host"` при старте возвращает понятную ошибку «host-бутылки поддерживаются начиная с M2».

## Review Focus

1. **Инъекция через id** (`"../evil"`, пробелы, кириллица в RPC/профиле) → отвергается валидацией до использования в netns/путях. Тест — Task 8 Step 1.
2. **WG-конфиг с DNS-endpoint** (не IP) → endpoint резолвится внутри namespace до `wg setconf`, иначе туннель молча не поднимется. Тест — Task 10 Step 1.
3. **Двойной `start` одной бутылки** (гонка CLI) → идемпотентность по спеке §5.2: второй вызов — no-op с предупреждением, не второй Up. Тест — Task 16 Step 1.
4. **Осираненные netns после краха демона / повторный Up того же id** → агент: `ALREADY_UP` на живой id; boot-cleanup сносит все `bottle-*`. Тест — Task 11 Step 1.
5. **Занятый порт на стороне VM при Up** → демон получает ошибку, переводит бутылку в `failed` и повторяет попытку с новыми портами один раз. Тест — Task 16 Step 3.

---

### Task 1: Скелет workspace + CI

**Files:**
- Create: `Cargo.toml` (workspace, members = crates/*), `rust-toolchain.toml`, `.gitignore` (target/, artifacts/)
- Create: `crates/core/src/lib.rs`, `crates/relay-lib/src/lib.rs`, `crates/agent/src/main.rs`, `crates/daemon/src/main.rs`, `crates/cli/src/main.rs` + `Cargo.toml` каждого крейта (зависимости по списку: tokio/serde/thiserror/tracing — в core и relay-lib; tokio+windows — daemon; clap — cli)
- Create: `.github/workflows/ci.yml`

**Interfaces:**
- Produces: workspace, в котором каждый следующий таск компилируется; CI-джобы `rust-checks` (ubuntu: fmt, clippy -D warnings, `cargo test --workspace`), `windows-checks` (windows-latest: clippy, build, test).

- [ ] **Step 1: Создать workspace и крейты-заглушки**

`crates/agent/src/main.rs` и `crates/daemon/src/main.rs` — `fn main() {}`; lib-крейты — пустые `pub mod`-заглушки не нужны, достаточно пустого файла. Крейт `relay-lib` — имя пакета `relay-lib`, lib-имя `relay_lib`; `core` — пакет `bottle-core` (lib `bottle_core`), чтобы не конфликтовать с чужими `core`.

- [ ] **Step 2: Убедиться, что сборка и проверки зелёные**

Run: `cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: PASS (пустые крейты).

- [ ] **Step 3: CI**

`ci.yml`: пуш/PR → джобы `rust-checks` (ubuntu-latest, actions/checkout@v4 + dtolnay/rust-toolchain@stable + Swatinem/rust-cache@v2; шаги fmt/clippy/test) и `windows-checks` (windows-latest, те же шаги без fmt).

- [ ] **Step 4: Коммит**

```bash
git add -A && git commit -m "chore(m0): rust workspace skeleton + CI"
```

---

### Task 2: `bottle-core::profile` — модель профиля

**Files:**
- Create: `crates/core/src/profile.rs` (+ `mod profile;` в lib.rs)
- Test: `crates/core/src/profile.rs` (unit, `#[cfg(test)]`)

**Interfaces:**
- Produces (используют Tasks 11, 13, 14, 16, 17):
  - `pub enum BottleKind { Env, Host }`, `pub enum TunnelProtocol { WireGuard, OpenVpn }` (serde rename kebab-case: `"env"`, `"host"`, `"wireguard"`, `"openvpn"`)
  - `pub struct VpnConfig` — serde tagged enum: `Env { protocol: TunnelProtocol, config: String }` (путь к файлу-копии в хранилище) | `Host { client: String, args: Option<String>, endpoints: Vec<String>, interface: Option<String> }`
  - `pub struct ForwardSpec { pub listen: u16, pub target: String }` (`target` — `host:port`, host может быть IP или DNS-имя)
  - `pub struct ExposeConfig { pub socks5: u16, pub forwards: Vec<ForwardSpec>, pub dns: Option<std::net::IpAddr>, pub launch: Option<String> }`
  - `pub struct BottleProfile { pub id: String, pub name: String, pub kind: BottleKind, pub vpn: VpnConfig, pub expose: ExposeConfig }`
  - `pub fn validate_id(id: &str) -> Result<(), ProfileError>`; `pub fn parse_profile(text: &str) -> Result<BottleProfile, ProfileError>` (парсит + валидирует); `pub fn render_profile(p: &BottleProfile) -> Result<String, ProfileError>`
  - `pub enum ProfileError` (thiserror): `InvalidId`, `InvalidPort`, `InvalidTarget`, `MissingField`, `Toml(toml::de::Error)`, …

- [ ] **Step 1: Написать падающие тесты**

Кейсы (по спеке §5.1): валидный env-профиль парсится (пример-ТOML прямо из спеки §5.1, секция kind="env", без закомментированного); id `"../evil"`, `"Офис"`, `""`, `"-x"` → `InvalidId`; порт `65536` → `InvalidPort`; target `"10.20.30.15"` (без порта) → `InvalidTarget`; `socks5 = 0` и отсутствующий `[expose]` → дефолты `socks5=0`, `forwards=[]`; round-trip `render(parse(t)) == t` для канонического t; host-профиль парсится (клиент+endpoints).

- [ ] **Step 2: Запустить, убедиться в падении**

Run: `cargo test -p bottle-core profile`
Expected: FAIL (модуль пуст).

- [ ] **Step 3: Реализовать через serde+toml**

serde: `#[serde(deny_unknown_fields)]` на структурах; дефолты через `#[serde(default)]`. Валидация — после парса в `parse_profile`.

- [ ] **Step 4: Прогнать тесты**

Run: `cargo test -p bottle-core`
Expected: PASS.

- [ ] **Step 5: Коммит**

```bash
git add crates/core && git commit -m "feat(core): bottle profile model with validation"
```

---

### Task 3: `bottle-core::protocol` — RPC демона и агента

**Files:**
- Create: `crates/core/src/protocol.rs`, `crates/core/src/frame.rs`
- Test: там же, `#[cfg(test)]`

**Interfaces:**
- Produces (Tasks 7, 11, 15, 16):
  - `pub const PROTOCOL_VERSION: u32 = 1;`
  - `frame.rs`: `pub fn encode_frame<T: Serialize>(&T) -> Vec<u8>` (u32 BE длина + JSON); `pub fn decode_frame<T: D>(&mut bytes: BytesMut) -> Result<Option<T>, FrameError>` — возвращает `Ok(None)` при неполном буфере (стиль tokio-util Decoder). Реализовать `pub struct LengthJsonCodec<T>(PhantomData<T>)` с `impl Decoder/Encoder` для использования с `Framed`.
  - `pub enum AgentRequest { Hello { version: u32, token: String }, Up(UpArgs), Down { id: String }, Status { id: Option<String> }, Shutdown }`
  - `pub struct UpArgs { pub id: String, pub protocol: TunnelProtocol, pub config: String, pub dns: Option<IpAddr>, pub socks5_port: u16, pub forwards: Vec<ForwardPort> }`
  - `pub struct ForwardPort { pub port: u16, pub target: String }`
  - `pub enum AgentReply { HelloAck { version: u32, appliance_version: String, vm_ip: Ipv4Addr }, Ready { id: String }, Status { bottles: Vec<BottleStatusInfo> }, Ok, Error { code: String, message: String } }`
  - `pub struct BottleStatusInfo { pub id: String, pub state: String, pub detail: Option<String> }`
  - Коды ошибок агента как константы: `pub mod err { pub const ALREADY_UP: &str = "ALREADY_UP"; pub const NOT_FOUND: &str = "NOT_FOUND"; pub const TUNNEL_FAILED: &str = "TUNNEL_FAILED"; pub const BAD_REQUEST: &str = "BAD_REQUEST"; pub const PORT_BUSY: &str = "PORT_BUSY"; pub const AUTH: &str = "AUTH"; pub const VERSION: &str = "VERSION"; }`

- [ ] **Step 1: Падающие тесты**

Round-trip каждого варианта запроса/ответа через codec (`encode → decode → assert_eq`); неполный буфер → `Ok(None)`; обрезанный JSON → `Err(FrameError::BadJson(..))`; `serde_json`-строка `UpArgs` содержит `"protocol":"wireguard"`.

- [ ] **Step 2: Запустить (FAIL) → Step 3: реализовать → Step 4: PASS**

Run: `cargo test -p bottle-core`

- [ ] **Step 5: Коммит**

```bash
git add crates/core && git commit -m "feat(core): agent rpc protocol + length-json codec"
```

---

### Task 4: `bottle-core::ipc` — RPC CLI и демона

**Files:**
- Create: `crates/core/src/ipc.rs`
- Test: там же

**Interfaces:**
- Produces (Tasks 14, 16, 17):
  - `pub const DAEMON_PIPE: &str = r"\\.\pipe\bottled";`
  - `pub enum CliRequest { List, Show { id: String }, Start { id: String }, Stop { id: String }, Status }`
  - `pub enum CliReply { List { bottles: Vec<BottleSummary> }, Show { profile: Option<BottleProfile>, runtime: Option<RuntimeState> }, Started { id: String }, Stopped { id: String }, Status { bottles: Vec<RuntimeState> }, Error { message: String } }`
  - `pub struct BottleSummary { pub id: String, pub name: String, pub kind: BottleKind, pub state: Option<String> }`
  - `pub enum BottleState { Starting, Up, Degraded, Failed { reason: String }, Stopping, Stopped }` (+ `pub fn as_str(&self) -> &'static str`)
  - `pub struct RuntimeState { pub id: String, pub state: BottleState, pub socks5: Option<u16>, pub forwards: Vec<ForwardPort> }`
  - Транспорт: тот же `LengthJsonCodec<T>` из Task 3.

- [ ] **Step 1: Падающие тесты** — round-trip каждого запроса/ответа; `BottleState::Failed{"туннель не поднялся".into()}.as_str() == "failed"`.
- [ ] **Step 2: FAIL → Step 3: реализовать (serde) → Step 4:** `cargo test -p bottle-core` PASS.
- [ ] **Step 5: Коммит**

```bash
git add crates/core && git commit -m "feat(core): cli<->daemon ipc types"
```

---

### Task 5: `relay-lib::forward` — TCP-проброс

**Files:**
- Create: `crates/relay-lib/src/forward.rs`, `crates/relay-lib/src/copy.rs`
- Test: там же

**Interfaces:**
- Consumes: ничего (первая часть relay-lib).
- Produces (Tasks 9, 11, 16):
  - `pub enum TargetAddr { Ip(SocketAddr), Domain { host: String, port: u16 } }` + `impl FromStr` (парсит `host:port`; host без `:` — ошибка; IPv6 в скобках `[::1]:3389`)
  - `pub async fn resolve_target(t: &TargetAddr) -> io::Result<SocketAddr>` (Domain — через `tokio::net::lookup_host`)
  - `pub async fn serve_forward(listener: TcpListener, target: TargetAddr) -> io::Result<()>` — принимает соединения, ставит `TCP_NODELAY`, копирует в обе стороны; ошибка одного соединения закрывает только его.
  - `pub async fn copy_bidirectional(a: TcpStream, b: TcpStream) -> io::Result<()>`

- [ ] **Step 1: Падающий тест (интеграционный, localhost)** — поднять tokio-echo; `serve_forward` на `127.0.0.1:0`; подключиться к слушателю, записать `"ping"`, прочитать `"ping"`; `TargetAddr::from_str("10.20.30.15:3389")` и `"[::1]:3389"` и `"host.example:443"` — ок; `"nohost"` — ошибка.
- [ ] **Step 2: FAIL → Step 3: реализовать (`tokio::io::copy_bidirectional` обёрнут в `copy_bidirectional`) → Step 4:** `cargo test -p relay-lib` PASS.
- [ ] **Step 5: Коммит**

```bash
git add crates/relay-lib && git commit -m "feat(relay-lib): tcp forward server + target addr"
```

---

### Task 6: `relay-lib::socks5` — SOCKS5-сервер

**Files:**
- Create: `crates/relay-lib/src/socks5.rs`
- Test: там же

**Interfaces:**
- Produces (Tasks 9, 11):
  - `pub async fn serve_socks5(listener: TcpListener) -> io::Result<()>` — RFC 1928, только no-auth (`0x00`) + `CONNECT`; адреса IPv4/IPv6/Domain; `BIND` и `UDP ASSOCIATE` → reply `0x07`; ошибку диала → reply `0x05` (connection refused) / `0x04` (host unreachable), соединение остаётся живым.

- [ ] **Step 1: Падающий тест** — echo-сервер; socks5 на `127.0.0.1:0`; hand-rolled клиент в тесте: `\x05\x01\x00` → `\x05\x00`; запрос CONNECT до echo (`\x01` + 4 байта IP + порт) → success `\x00`; отправка/приём данных сквозь SOCKS; CONNECT до закрытого порта → `\x05`; команда `0x02` (BIND) → `\x07`; Domain-адрес (`\x03` + len + `localhost`) до echo → `\x00`.
- [ ] **Step 2: FAIL → Step 3: реализовать (ручной парсер байтов, `resolve_target` из Task 5 для Domain) → Step 4:** `cargo test -p relay-lib` PASS.
- [ ] **Step 5: Коммит**

```bash
git add crates/relay-lib && git commit -m "feat(relay-lib): socks5 CONNECT server"
```

---

### Task 7: `agent` — control-сервер: Hello/auth/dispatch

**Files:**
- Create: `crates/agent/src/main.rs` (clap-подкоманды `serve`, `relay`, `--boot`), `crates/agent/src/control.rs`, `crates/agent/src/token.rs`
- Test: `crates/agent/src/control.rs` `#[cfg(test)]` (транспорт абстрагирован)

**Interfaces:**
- Consumes: `AgentRequest/AgentReply/err/PROTOCOL_VERSION`, `LengthJsonCodec` (Task 3).
- Produces (Tasks 9–11):
  - `pub struct ControlConfig { pub token_path: PathBuf }` (по умолчанию `/etc/bottle-agent/token`)
  - `pub fn load_or_create_token(path: &Path) -> io::Result<String>` — если файла нет: 32 случайных байта hex, записать с правами 0600 (unix `PermissionsExt`), вернуть; если есть — прочитать.
  - `pub async fn handle_request(state: &AgentState, req: AgentRequest) -> AgentReply` — чистая функция поверх состояния: `Hello` сверяет версию (`!= PROTOCOL_VERSION` → `Error{VERSION}`) и токен (`!= state.token` → `Error{AUTH}`), иначе `HelloAck { version, appliance_version: env!("CARGO_PKG_VERSION"), vm_ip: state.vm_ip }`; остальные запросы до успешного `Hello` → `Error{AUTH}`; неизвестные → `Error{BAD_REQUEST}`.
  - `pub struct AgentState { pub token: String, pub vm_ip: Ipv4Addr, pub authed: AtomicBool, pub bottles: Bottles }` (тип `Bottles` доопределяет Task 11).
  - `serve`: TCP-сервер `0.0.0.0:7777`, один клиент, `Framed` с codec.

- [ ] **Step 1: Падающие тесты (unit, без сокетов)** — `handle_request`: верный Hello+token → HelloAck c `vm_ip` из state; неверный token → `err::AUTH`; до Hello запрос `Status` → `AUTH`; Hello с `version: 99` → `VERSION`; после authed — `Status` обрабатывается (пустой список). Тест `load_or_create_token`: tempdir, первый вызов создаёт файл с hex-строкой ≥64 симв., повторный возвращает тот же.
- [ ] **Step 2: FAIL → Step 3: реализовать (clap: подкоманды serve/relay + флаг --boot пока no-op с TODO-выводом? — нет: `--boot` реализуется в Task 11, здесь заглушка `unimplemented` запрещена: подкоманда печатает «реализовано в Task 11» и выходит кодом 0? Нет — правильно: в этом таске `--boot` и `relay` отсутствуют в clap вовсе, добавит Task 9/11) → Step 4:** `cargo test -p agent` PASS (только unit-тесты control/token).
- [ ] **Step 5: Коммит**

```bash
git add crates/agent && git commit -m "feat(agent): control server auth + version handshake"
```

---

### Task 8: `agent::netns` — чистая логика имён и адресов

**Files:**
- Create: `crates/agent/src/netns.rs` (только чистые функции; syscall-обёртки добавит Task 9)
- Test: там же

**Interfaces:**
- Produces (Tasks 9, 10, 11):
  - `pub fn netns_name(id: &str) -> Result<String, AgentError>` — `format!("bottle-{id}")`; id обязан пройти `bottle_core::profile::validate_id`, иначе `BAD_REQUEST` (это барьер инъекций: имя netns становится частью путей `/run/netns/...` и `/etc/netns/...`).
  - `pub struct Slot { pub idx: u8, pub peer: Ipv4Addr, pub inner: Ipv4Addr, pub subnet: Ipv4Addr }` — для `idx`: peer=`10.77.idx.1`, inner=`10.77.idx.2`, subnet=`10.77.idx.0/30`.
  - `pub struct SlotTable { used: BTreeSet<u8> }` — `pub fn alloc(&mut self) -> Result<Slot, AgentError>` (первый свободный 1..=250, иначе `BAD_REQUEST "нет свободных слотов"`), `pub fn free(&mut self, idx: u8)`, `pub fn len(&self)`.
  - `pub fn veth_names(idx: u8) -> (String, String)` — (`format!("p{idx}")`, `format!("b{idx}")`).

- [ ] **Step 1: Падающие тесты** — `netns_name("../evil")` и `netns_name("офис")` → Err; `netns_name("office-a")` → `Ok("bottle-office-a")`; SlotTable: alloc 1..3 монотонно, free(2) → следующий alloc снова 2; исчерпание (used = 1..=250) → Err; `veth_names(7)` = (`"p7"`, `"b7"`).
- [ ] **Step 2: FAIL → Step 3: реализовать (валидация — ручная проверка charset, без regex) → Step 4:** PASS.
- [ ] **Step 5: Коммит**

```bash
git add crates/agent && git commit -m "feat(agent): netns naming and slot allocation"
```

---

### Task 9: `agent::netns` — жизненный цикл + relay в namespace + мост

**Files:**
- Modify: `crates/agent/src/netns.rs` (добавить syscall-слой)
- Create: `crates/agent/src/relay_run.rs` (подкоманда `relay`)
- Test: `crates/agent/tests/netns_lifecycle.rs` — `#[cfg(target_os = "linux")]`, `#[ignore]` (нужен root; CI: sudo)

**Interfaces:**
- Consumes: `Slot/SlotTable/netns_name/veth_names` (Task 8), `serve_socks5/serve_forward/TargetAddr` (Tasks 5–6).
- Produces (используют Tasks 10, 11):
  - `pub struct NetnsHandle { pub id: String, pub slot: Slot, pub relay_child: Child, pub bridge_tasks: JoinSet<()> }`
  - `pub fn netns_up(slot: &Slot, id: &str, listeners: &[ForwardPort], socks5_port: u16) -> Result<NetnsHandle, AgentError>` — shell-последовательность через `std::process::Command` (`ip` из iproute2): `ip netns add`; veth create/move; адреса peer/inner; links up; default route в ns через peer (чтобы endpoint/диал были достижимы до туннеля); запуск relay внутри ns: `ip netns exec <ns> /proc/self/exe relay --socks5-port <p> --forward <port>:<target>...`; мосты в default ns: на каждый listener — `serve_forward` на `0.0.0.0:<port>` → `TargetAddr::Ip(10.77.idx.2:<port>)`.
  - `pub fn netns_down(h: NetnsHandle) -> Result<(), AgentError>` — kill relay-child, `ip netns del` (удаляет veth), `/etc/netns/<ns>/resolv.conf` удалить.
  - `pub fn set_dns(id: &str, dns: Option<IpAddr>) -> Result<(), AgentError>` — пишет `/etc/netns/bottle-<id>/resolv.conf` (`nameserver <ip>`; None → файл с `nameserver` от VM default, чтобы DNS в ns не смотрел наружу).
  - `relay_run`: `pub async fn run_relay(socks5_port: u16, forwards: Vec<ForwardPort>) -> io::Result<()>` — слушает `0.0.0.0` внутри ns (SOCKS5 + forwards на те же порты).
- [ ] **Step 1: Падающий интеграционный тест (root)**

`netns_lifecycle.rs`: вызвать `netns_up(slot, "t-a", listeners=[], socks5_port=49200)` и проверить: (а) `ip netns list` содержит `bottle-t-a`; (б) TCP-соединение к мосту `127.0.0.1:49200` устанавливается и SOCKS5-handshake `\x05\x01\x00` → `\x05\x00` проходит (значит relay жив внутри namespace; полноценный сквозной обмен данными появится в Task 10, где есть «туннель» до цели); (в) после `netns_down` — `ip netns list` пуст, veth `p<idx>` отсутствует (`ip link show` rc≠0), повторный `netns_up` на том же слоте работает.

- [ ] **Step 2: Запустить и убедиться в падении**

Run: `sudo -E cargo test -p agent --test netns_lifecycle -- --ignored` (linux)
Expected: FAIL (функции нет).

- [ ] **Step 3: Реализовать**

`relay` как clap-подкоманда в main.rs (Task 7 расширяется: подкоманды `serve`|`relay`, флаг `--boot`). `/proc/self/exe` для запуска себя в ns.

- [ ] **Step 4:** та же команда — PASS. Также `cargo clippy --workspace -- -D warnings` — PASS.
- [ ] **Step 5: Коммит**

```bash
git add crates/agent && git commit -m "feat(agent): netns lifecycle with relay inside namespace"
```

---

### Task 10: `agent::tunnel` — WireGuard в namespace

**Files:**
- Create: `crates/agent/src/wg.rs` (парсер конфига + запуск)
- Test: `crates/agent/tests/wg_handshake.rs` (`#[cfg(linux)] #[ignore]`, root)

**Interfaces:**
- Consumes: `NetnsHandle/set_dns` (Task 9), `TunnelProtocol` (Task 2).
- Produces (Task 11):
  - `pub struct ParsedWg { pub interface_addr: Ipv4Addr, pub endpoint_host: Option<String>, pub endpoint_port: u16, pub setconf_body: String }` — из `.conf` (`[Interface] Address/PrivateKey/ListenPort?`, `[Peer] PublicKey/AllowedIPs/Endpoint`): Address без префикса `/32`, Endpoint выносится отдельно (в `setconf_body` НЕ попадает — `wg setconf` требует IP-endpoint), AllowedIPs сохраняются.
  - `pub fn parse_wg_config(text: &str) -> Result<ParsedWg, AgentError>`
  - `pub async fn resolve_endpoint_in_ns(ns: &str, host: &str) -> Result<Ipv4Addr, AgentError>` — `ip netns exec <ns> getent ahostsv4 <host>` (первая строка); host уже IP — вернуть как есть. Это закрывает Review Focus #2.
  - `pub async fn tunnel_up_wg(h: &NetnsHandle, cfg: ParsedWg) -> Result<(), AgentError>` — в ns: `wireguard-go wg0` (spawn, лог в tmpfs); `ip link set wg0 up`; `ip addr add <addr>/32 dev wg0`; endpoint-хост DNS → IP (Review Focus #2); `wg setconf wg0 <setconf+Endpoint=ip:port>`; маршрут до endpoint `/32` через peer-gateway `10.77.idx.1`; маршруты `0.0.0.0/1` и `128.0.0.0/1` via `wg0`; handshake-wait ≤30 с: `wg show wg0 latest-handshakes` → значение >0, иначе `err::TUNNEL_FAILED`.
  - `pub fn tunnel_down(h: &NetnsHandle)` — kill wireguard-go-процесса (PID хранится в handle: расширить `NetnsHandle` полем `tunnel_child: Option<Child>`).

- [ ] **Step 1: Падающие тесты**

Unit (`wg.rs`): `parse_wg_config` на фикстуре с `Endpoint = vpn.example.com:51820` и `AllowedIPs = 0.0.0.0/0, ::/0` — `endpoint_host = Some("vpn.example.com")`, порт 51820, в `setconf_body` НЕТ `Endpoint`, ЕСТЬ `AllowedIPs`; `Address = 10.8.0.2/32` → `interface_addr = 10.8.0.2`; мусорный конфиг → Err.
Интеграционный (`wg_handshake.rs`, root): два netns — `srv` и наш `bottle-t-wg`, соединённые veth `10.77.253.0/30` (вне диапазона слотов); в `srv`: `wireguard-go wg1` со сгенерированной парой ключей (ключи фиксированные в тесте, `wg genkey` на стенде), `AllowedIPs = 10.99.0.2/32`; в `bottle-t-wg` через `netns_up` + `tunnel_up_wg` с peer-ключами и `Endpoint = 10.77.253.1:51820` (IP — обходим resolve), `AllowedIPs = 10.99.0.1/32`; assert: handshake >0 в течение 30 с; SOCKS5 CONNECT через мост к `10.99.0.1:<echo-порт srv>` возвращает `\x00` и данные доходят (echo поднят в `srv`).

- [ ] **Step 2: FAIL (обе группы) → Step 3: реализовать → Step 4:**

Run: `cargo test -p agent wg && sudo -E cargo test -p agent --test wg_handshake -- --ignored`
Expected: PASS.

- [ ] **Step 5: Коммит**

```bash
git add crates/agent && git commit -m "feat(agent): wireguard tunnel in namespace with handshake wait"
```

---

### Task 11: `agent::bottles` — полный Up/Down + boot-cleanup

**Files:**
- Create: `crates/agent/src/bottles.rs`
- Modify: `crates/agent/src/main.rs` (`--boot`), `crates/agent/src/control.rs` (dispatch реальных операций)
- Test: `crates/agent/tests/bottles_e2e.rs` (`#[cfg(linux)] #[ignore]`, root)

**Interfaces:**
- Consumes: всё из Tasks 7–10.
- Produces (Task 15, 16):
  - `pub struct Bottles { inner: Mutex<HashMap<String, NetnsHandle>>, slots: Mutex<SlotTable> }`
  - `impl Bottles`: `pub async fn up(&self, args: UpArgs) -> AgentReply` — `netns_name` (барьер Task 8), если id жив → `err::ALREADY_UP` (Review Focus #4); `slot alloc`; `set_dns`; `netns_up`; `tunnel_up_wg` (или для `OpenVpn`: spawn `openvpn --config <tmpfs-путь> --dev tun0` в ns, туннель-готовность = existence `tun0` + лог «Initialization Sequence Completed» ≤60 с); конфиг пишется в `/run/bottles/<id>/` (tmpfs) и удаляется при down; `Ready{id}`.
  - `pub async fn down(&self, id: &str) -> AgentReply` — `tunnel_down` + `netns_down` + slot free + tmpfs rm; отсутствующий id → `err::NOT_FOUND`.
  - `pub fn status(&self, id: Option<&str>) -> Vec<BottleStatusInfo>`
  - `pub fn boot_cleanup()` — удалить все `/run/netns/bottle-*` и `/etc/netns/bottle-*`, убить потомков-relay/wireguard-go по PID-файлам `/run/bottles/*/`; iptables: `iptables -t nat -C POSTROUTING -s 10.77.0.0/16 -o eth0 -j MASQUERADE || -A ...` (SNAT внешних пакетов туннелей при выходе из VM; Review Focus потерь нет — MASQUERADE ставится один раз на boot).
  - `--boot` в main: `boot_cleanup()` затем serve.
- [ ] **Step 1: Падающий интеграционный тест**

`bottles_e2e.rs` (стенд как в Task 10, server-netns): `Bottles::up(UpArgs{id:"t-e2e", protocol:WireGuard, config: <клиентский конфиг>, dns:None, socks5_port:49210, forwards: vec![ForwardPort{port:49211, target:"10.99.0.1:<echo>"}]})` → `Ready`; `Status` → state "up"; повторный `up` тот же id → `err::ALREADY_UP` (Review Focus #4); `down("t-e2e")` → `Ok`; после down `ip netns list` без bottle-*; `up` другого id → работает (слоты свободны). Плюс unit-тест: `up` с id `"../evil"` → `BAD_REQUEST` (Review Focus #1, дублирует барьер на уровне API).

- [ ] **Step 2: FAIL → Step 3: реализовать → Step 4:** `sudo -E cargo test -p agent --test bottles_e2e -- --ignored` PASS; `cargo test --workspace` PASS.
- [ ] **Step 5: Коммит**

```bash
git add crates/agent && git commit -m "feat(agent): bottle up/down api, boot cleanup, masquerade"
```

---

### Task 12: Апплайенс — rootfs и сборка

**Files:**
- Create: `appliance/Dockerfile`, `appliance/build-rootfs.sh`, `appliance/overlay/etc/wsl.conf`, `appliance/overlay/etc/bottle-appliance-version` (= версия `bottle-core` CARGO_PKG_VERSION, генерируется скриптом), `appliance/README.md`
- Test: ручная проверка + CI-джоба `appliance-build`

**Interfaces:**
- Consumes: бинарник `bottle-agent` (target `x86_64-unknown-linux-musl`).
- Produces (Tasks 15, 18): артефакт `artifacts/bottleforapps-rootfs.tar.gz` (rootfs Alpine + пакеты `wireguard-go wireguard-tools openvpn iproute2 iptables` + `/bin/bottle-agent` + `/etc/wsl.conf` с `[boot]\ncommand=/bin/bottle-agent --boot` + version-файл).

- [ ] **Step 1: Dockerfile + скрипт**

`build-rootfs.sh`: внутри docker (alpine:3.20): rustup + musl-target, `cargo build --release -p agent --target x86_64-unknown-linux-musl`; `apk add` список пакетов; копирование overlay + бинарника; `docker export` → tar.gz. Версия: `grep version crates/core/Cargo.toml` → overlay-файл.

- [ ] **Step 2: Проверить сборку и smoke**

Скрипт тегирует собранный образ `bottleforapps-appliance:latest`, гоняет smoke внутри него, затем экспортирует контейнер в tar.gz. Run: `./appliance/build-rootfs.sh && docker run --rm --entrypoint /bin/sh bottleforapps-appliance -c '/bin/bottle-agent --help && cat /etc/bottle-appliance-version'`
Expected: tar.gz создан; `--help` печатает подкоманды serve/relay. (Если docker недоступен локально — джоба CI.)

- [ ] **Step 3: CI-джоба `appliance-build`** (ubuntu, docker), загружает `bottleforapps-rootfs.tar.gz` как artifact. В `ci.yml`.

- [ ] **Step 4: Коммит**

```bash
git add appliance .github && git commit -m "build(appliance): alpine rootfs with bottle-agent"
```

---

### Task 13: `daemon` — хранилище профилей

**Files:**
- Create: `crates/daemon/src/store.rs`
- Test: там же (`#[cfg(test)]`, tempdir)

**Interfaces:**
- Consumes: `BottleProfile/parse_profile/render_profile` (Task 2).
- Produces (Tasks 14, 16):
  - `pub struct Store { root: PathBuf }`; `pub fn open(root: &Path) -> io::Result<Store>` — создаёт `bottles/`, `logs/`; на unix права 0700 на root (секреты, спека §10).
  - `pub fn list(&self) -> Vec<(String, io::Result<BottleProfile>)>` — все `bottles/*.toml` (id = имя файла, сверяется с полем `id`).
  - `pub fn load(&self, id: &str) -> Result<BottleProfile, DaemonError>`; `pub fn save(&self, p: &BottleProfile) -> Result<(), DaemonError>` (+ для `VpnConfig::Env` копирует конфиг-файл в `bottles/<id>.conf` при создании — принимает исходный путь в `config` и нормализует).
  - `pub fn appliance_token_path(&self) -> PathBuf` (root/`appliance-token`), `pub fn data_root(&self) -> &Path`.

- [ ] **Step 1: Падающие тесты** — tempdir: save+load round-trip; `list` после двух save; load несуществующего → Err; файл с невалидным id внутри → `list` возвращает Err для него, остальные живы; на unix — права 0700.
- [ ] **Step 2: FAIL → Step 3: реализовать → Step 4:** `cargo test -p daemon` PASS.
- [ ] **Step 5: Коммит**

```bash
git add crates/daemon && git commit -m "feat(daemon): profile store"
```

---

### Task 14: `daemon` — named pipe сервер + single-instance + диспетчер

**Files:**
- Create: `crates/daemon/src/pipe.rs`, `crates/daemon/src/main.rs` (аргумент `--data-dir`)
- Test: `crates/daemon/src/pipe.rs` (`#[cfg(windows)] #[cfg(test)]`)

**Interfaces:**
- Consumes: `CliRequest/CliReply/DAEMON_PIPE` (Task 4), `Store` (Task 13).
- Produces (Tasks 16, 17): `pub struct Daemon { pub store: Store, pub manager: Option<Arc<BottleManager>> }` + `pub async fn serve_daemon(data_dir: PathBuf, shutdown: CancellationToken)` — named pipe loop (tokio `windows::named_pipe`). Диспетчер: `List/Show` обслуживает из Store; `Start/Stop/Status` маршрутизирует в `manager`, если он подключён, иначе отвечает `CliReply::Error { message: "команда ещё не подключена (Task 16)" }` — Task 16 внедряет реальный `BottleManager` и замещает этот переходный ответ.
  - Single-instance: `windows` crate `CreateMutexW` имени `Global\bottled-singleton`; второй запуск — выход с сообщением «демон уже запущен».
- [ ] **Step 1: Падающий тест (windows)** — in-process: создать `Daemon` с tempdir-store, сохранить 2 профиля, поднять pipe-сервер на `\\.\pipe\bottled-test-<rand>`, клиентом послать `List` → 2 записи, `Show{id}` → профиль, `Show` несуществующего → `Show{profile: None}`.
- [ ] **Step 2: FAIL → Step 3: реализовать → Step 4:** `cargo test -p daemon` PASS на windows (CI windows-checks); на linux крейт собирается (pipe-модуль `#[cfg(windows)]`).
- [ ] **Step 5: Коммит**

```bash
git add crates/daemon && git commit -m "feat(daemon): named pipe server, list/show, single instance"
```

---

### Task 15: `daemon::appliance` — импорт, токен, соединение

**Files:**
- Create: `crates/daemon/src/appliance.rs`
- Test: там же (фейковый раннер команд)

**Interfaces:**
- Consumes: `Store::appliance_token_path/data_root` (Task 13), `AgentRequest/Reply/PROTOCOL_VERSION` (Task 3), tarball из Task 12 (путь передаётся).
- Produces (Task 16):
  - `#[async_trait] pub trait WslRunner { async fn run(&self, args: &[&str], stdin: Option<Vec<u8>>) -> io::Result<String>; }` + `pub struct RealWsl;` (вызывает `wsl.exe`, кодировка UTF-8 флагом `--exec`/`WSL_UTF8=1`).
  - `pub async fn ensure_appliance(wsl: &dyn WslRunner, store: &Store, rootfs_tar: &Path) -> Result<ApplianceHandle, DaemonError>` — шаги: `wsl -l -q` содержит `bottleforapps`? нет → сгенерировать 32-байта hex-токен, `wsl --import bottleforapps <data>/appliance-vm <tar>`; записать токен в VM: `wsl -d bottleforapps --exec /bin/sh -c 'cat > /etc/bottle-agent/token'` (stdin), сохранить локально `appliance-token` (0600); wake: `wsl -d bottleforapps --exec /bin/true`; узнать IP: `wsl -d bottleforapps --exec hostname -I` (первый адрес); TCP-коннект `vm_ip:7777` c ретраями ≤30 с; `Hello{version, token}` → `HelloAck`; `version != PROTOCOL_VERSION` → ошибка «выполните bottle upgrade (апплайенс устарел)».
  - `pub struct ApplianceHandle { pub vm_ip: Ipv4Addr, pub conn: Framed<TcpStream, LengthJsonCodec<AgentReply>> }` — методы `pub async fn send(&mut self, req: AgentRequest) -> Result<AgentReply, DaemonError>`, `pub async fn alive(&self) -> bool` (TCP peek/ping `Status`).

- [ ] **Step 1: Падающие тесты (фейк WslRunner)** — фейк ведёт журнал вызовов + поднимает локальный TCP-echo-agent (реальный `Framed`-сервер в тесте, отвечающий `HelloAck`/`Error{VERSION}` по сценарию): (а) свежий store → последовательность вызовов: `--import`, stdin-запись токена, wake, hostname; (б) повторный `ensure` на «уже импортировано» — без import; (в) агент с версией 99 → DaemonError с текстом «upgrade»; (г) агент не отвечает 30 с → DaemonError с таймаутом (в тесте таймаут параметром, 100 мс).
- [ ] **Step 2: FAIL → Step 3: реализовать → Step 4:** `cargo test -p daemon` PASS.
- [ ] **Step 5: Коммит**

```bash
git add crates/daemon && git commit -m "feat(daemon): appliance import, token provisioning, agent connection"
```

---

### Task 16: `daemon::manager` — жизненный цикл env-бутылки (оркестрация §7.1)

**Files:**
- Create: `crates/daemon/src/manager.rs`
- Modify: `crates/daemon/src/pipe.rs` (диспетчер Start/Stop/Status)
- Test: `crates/daemon/src/manager.rs` `#[cfg(test)]` (фейковый агент)

**Interfaces:**
- Consumes: `ApplianceHandle` (Task 15), `Store` (Task 13), `serve_forward/TargetAddr` (Task 5), `RuntimeState/BottleState` (Task 4).
- Produces (Task 17):
  - `#[async_trait] pub trait AgentConnect { async fn connect(&self) -> Result<ApplianceHandle, DaemonError>; }` — точка инъекции для тестов; продакшн-реализация `RealAgentConnect` оборачивает `ensure_appliance` (Task 15) с кэшем живого соединения и переподключением.
  - `pub struct BottleManager { store: Store, connect: Arc<dyn AgentConnect>, runtime: Mutex<HashMap<String, Runtime>> }`
  - `pub struct Runtime { pub state: BottleState, pub socks5_listener: TcpListener, pub forward_listeners: Vec<(u16, TcpListener)>, pub tasks: JoinSet<()> }`
  - `impl BottleManager`: `pub async fn start(&self, id: &str) -> Result<CliReply, DaemonError>`; `pub async fn stop(&self, id: &str) -> ...`; `pub fn status(&self) -> Vec<RuntimeState>`
  - Алгоритм `start` (спека §7.1): профиль из store; `kind/host` → `CliReply::Error{"host-бутылки поддерживаются начиная с M2"}`; уже `Starting/Up` → предупреждение-noop (Review Focus #3); резервирование портов: `TcpListener::bind("127.0.0.1:0")` в цикле пока порт ∈ 49152..=65535, держать слушатели занятыми; `ensure_appliance`; `Up{id, protocol, config: <содержимое файла>, dns, socks5_port, forwards}`; `Ready` → заменить заглушенные слушатели на `serve_forward(127.0.0.1:port → vm_ip:port)`; state=Up; выполнить `expose.launch` (spawn, вывод не ждём). `Error{PORT_BUSY}` от агента → перевыбор портов и повтор Up один раз (Review Focus #5); прочие ошибки → state=Failed{reason}, слушатели закрыть.
  - `stop`: `Down{id}` агенту, погасить задачи/слушатели, state=Stopped.
- [ ] **Step 1: Падающие тесты (через trait `AgentConnect`)**

В тесте — фейк-реализация `AgentConnect`: in-process TCP-сервер, говорящий по протоколу агента, + настоящий echo-сервер «за туннелем». Сценарии: (1) полный старт: профиль → фейк-агент `Ready` → `127.0.0.1:<socks5>` соединяется и SOCKS5-handshake проходит, данные до echo; статус `up`; (2) повторный `start` на `up` → no-op + в статусе одна бутылка (Review Focus #3); (3) агент отвечает `Error{PORT_BUSY}` → port-ретрай: второй Up с другими портами, итог `up` (Review Focus #5); (4) агент `Error{TUNNEL_FAILED}` → state `failed{reason}` и порт свободен (можно снова bind); (5) `stop` → агенту ушёл `Down`, слушатели закрыты (connect → refused).

- [ ] **Step 2: FAIL → Step 3: реализовать → Step 4:** `cargo test -p daemon` PASS (windows CI; linux-совместимый код без cfg — tokio TCP кроссплатформенный).
- [ ] **Step 5: Коммит**

```bash
git add crates/daemon && git commit -m "feat(daemon): env bottle lifecycle orchestration"
```

---

### Task 17: `cli` — bottle.exe

**Files:**
- Create: `crates/cli/src/main.rs`, `crates/cli/src/render.rs`
- Test: `crates/cli/src/render.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: `CliRequest/CliReply/DAEMON_PIPE` (Task 4), `LengthJsonCodec` (Task 3).
- Produces: бинарник `bottle` с командами `list|show <id>|start <id>|stop <id>|status` (clap); подключение к pipe `DAEMON_PIPE`; вывод на русском; ошибки демона печатаются и код выхода 1.

- [ ] **Step 1: Падающие тесты рендера** — `render_status(vec![RuntimeState{id:"office-a", state:Up, socks5:Some(55001), forwards:[(55002,"10.20.30.15:3389")]}])` содержит строки `office-a`, `up`, `socks5 127.0.0.1:55001`, `rdp 10.20.30.15:3389 → 127.0.0.1:55002`; `render_list` с `state: None` показывает `-`.
- [ ] **Step 2: FAIL → Step 3: реализовать (clap derive; клиент named pipe windows; логика thin) → Step 4:** `cargo test -p cli` PASS, `cargo build --workspace` PASS.
- [ ] **Step 5: Коммит**

```bash
git add crates/cli && git commit -m "feat(cli): bottle list/show/start/stop/status"
```

---

### Task 18: WSL-интеграционная CI + чеклист приёмки M1

**Files:**
- Modify: `.github/workflows/ci.yml` (джоба `wsl-integration`, windows-latest, `continue-on-error: true` в M1)
- Create: `scripts/ci-wsl-integration.ps1`, `docs/checklists/m1-verification.md`, `README.md` (корневой, краткий: сборка, первый запуск, создание env-бутылки)

**Interfaces:**
- Consumes: артефакт Task 12, бинарники Task 14–17.

- [ ] **Step 1: CI-сценарий** — windows-раннер: `wsl --install --no-distribution` (если нужно); собрать workspace; скачать artifact rootfs; запустить `bottled --data-dir <temp>`; создать профиль с WG-конфигом, endpoint которого — «сервер», поднятый в той же WSL-VM вторым distro-запуском `bottle-agent serve`-вариантом стенда (стенд Task 10 переносится в VM через `wsl --exec`); `bottle start`; assert `bottle status` = `up` и TCP через socks5 доходит до echo; `bottle stop`; assert слушатели закрыты. Скрипт идемпотентен, `continue-on-error: true` до приёмки M2 (спека §15 — фолбэк на Linux-тесты).
- [ ] **Step 2: Чеклист ручной приёмки M1** (спека §14): две бутылки с разными WG-конфигами одновременно; каждая SOCKS5 выходит через свой туннель (проверка внешним IP через socks5: `curl --socks5 127.0.0.1:PORT https://api.ipify.org`); endpoint'ы идут через физику (на стороне WG-сервера виден IP физической сети, не соседнего туннеля); RDP через TCP-проброс (`mstsc /v:127.0.0.1:PORT`); `bottle stop` убирает netns без остатков (`wsl -d bottleforapps -- ip netns list` пуст).
- [ ] **Step 3: README** — сборка (cargo build --workspace; appliance/build-rootfs.sh), первый запуск bottled, минимальный профиль env.
- [ ] **Step 4: Коммит**

```bash
git add .github scripts docs README.md && git commit -m "ci(wsl): integration job + m1 acceptance checklist"
```

---

## Что НЕ входит в этот план (последующие планы)

- **M2** — `bottle-svc` (служба маршрутов, named pipe, журнал+сверка), host-бутылки (endpoints/DNS-цикл 30 с, ожидание интерфейса 120 с, `IP_UNICAST_IF`), устойчивость демона (резюме сна, авто-восстановление env-бутылок), `routes cleanup`.
- **M3** — MSI (WiX) + portable, `doctor`, `logs export`, маскирование журналов в полном объёме, документация пользователя.
- **M4** — Tauri GUI, macOS-порт апплайнса.

Каждый этап получит свой план после приёмки предыдущего (решения M2/M3 зависят от уроков M1).
