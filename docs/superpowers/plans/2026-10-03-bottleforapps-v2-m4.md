# BottleForApps v2 — M4 Implementation Plan (GUI + macOS-порт)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Графический интерфейс (Tauri) для управления бутылками поверх существующего демона; рабочий порт на macOS (env-бутылки через Virtualization.framework-VM с тем же Linux-аплаенсом).

**Architecture:** Два этапа с гейтом между ними. Этап A (GUI): Tauri-обёртка, управляющая демоном через `bottle --json` (сайдкар-подпроцесс) — демон и RPC не меняются, GUI работает на обеих ОС бесплатно. Этап B (macOS): транспорт CLI↔демона абстрагируется (named pipe → Unix domain socket), WslRunner заменяется VzRunner (Virtualization.framework через крейт `vz`), тот же Alpine-rootfs в arm64.

**Tech Stack:** Tauri 2 (Rust + vanilla TS/Vite без UI-фреймворков), `vz`-крейт (биндинги Virtualization.framework; fallback — Swift-хелпер, см. Ruling D3), appliance multi-arch (aarch64/x86_64 musl).

**Spec:** [docs/superpowers/specs/2026-09-30-bottleforapps-v2-design.md](../specs/2026-09-30-bottleforapps-v2-design.md) — §2 (macOS-порт — M4; GUI — M4), §14 («M4: Tauri GUI, macOS-порт апплайнса — отдельная спека»). НИЖЕ — проектные рулинги, заменяющие отдельную спеку; их утверждение = утверждение этого плана.

## Проектные рулинги (утверждаются вместе с планом — это «отдельная спека» M4)

- **D1. GUI управляет через `bottle --json`.** CLI получает флаг `--json`: все команды печатают machine-readable `CliReply` (JSON) вместо текста, коды выхода стабильны (0 ок / 1 ошибка / 2 парсинг). GUI (Tauri) — сайдкар-подпроцесс: спавнит `bottle --json`, парсит, рендерит. Демон, именованные каналы и RPC не трогаем вообще — нулевая граница изменений; статус-поллинг 2 с. Отвергнутая альтернатива: TCP-транспорт демона для GUI (новая поверхность атаки, второй RPC-путь). Стоимость решения: спавн процесса на действие — для GUI-взаимодействий незаметно.
- **D2. macOS = только env-бутылки.** Host-бутылки (фирменные клиенты + служба маршрутов) остаются Windows-эксклюзивом: их механика целиком про Windows-таблицу маршрутов. На macOS profile kind=host даёт понятную ошибку. Это паритет с M1-объёмом, не с M2.
- **D3. VM на macOS — крейт `vz` (Virtualization.framework), go/no-go-спайк.** Этап B начинается задачей-спайком с жёсткими критериями (Task 9); при провале `vz` — вендорный Swift-хелпер (собирается в CI macOS-раннером, Xcode там есть); при провале обоих — M4b откладывается с отчётом владельцу, НЕ молча. IP-адрес VM: агент объявляет себя UDP-broadcast'ем в vmnet-мосту (токен-подписанный), демон слушает и подключается control-TCP — та же модель «VM_IP:7777», что в WSL.
- **D4. GUI-объём M4:** окно-список бутылок (статус/порты/кнопки старт-стоп), форма create/edit (те же поля, что `bottle create`), doctor-панель, кнопка logs export, tray-меню (старт/стоп/статус). НЕ входит: редактор конфигов VPN (файл выбирается), автообновление, мультиязычность (русский), тёмная тема (системная — Tauri default).
- **D5. Дистрибуция macOS:** `.app`-бандл (Tauri) с вложенными bottled/bottle/rootfs-arm64 в Resources; установка = перетащить в Программы;卸 = удалить. Подпись/нотаризация — как с MSI: осознанно вне плана (каркас в CI закомментирован). Windows-GUI входит в состав MSI/portable M3 как отдельный exe `bottle-gui.exe` (добавляется релизной джобой).

## Global Constraints

- Всё действующее: Rust stable; fmt + clippy -D warnings; русские строки; телеметрии нет; секреты не логируются.
- `--json`-контракт: стабильные имена полей = имена `CliReply` (serde rename_all="snake_case"); в JSON не появляются секреты (профиль несёт имя конфига, не содержимое) — тест-дубль.
- UDS-транспорт (macOS): сокет `<data>/daemon.sock`, права 0600, каталог 0700 — прав достаточно, токен поверх не нужен (ruling).
- cfg-гигиена: macOS-сборка не тянет windows-only код и наоборот; гейты CI: ubuntu (тест), windows (msvc-check), macos-14 (arm64 host test + aarch64-apple-darwin check) + musl x86_64/aarch64 для агента.
- appliance multi-arch: `build-rootfs.sh <x86_64|aarch64>`; WSL-путь берёт x86_64, macOS — aarch64; артефакты различаются суффиксом.
- GUI не дублирует бизнес-логику: всё через CLI; в GUI нет криптографии/сети кроме спавна `bottle`.

## Review Focus

1. **`--json` нестабилен/утекает** (поля переименованы, конфиг-содержимое в ответах) — контракт запинен golden-тестами. Тест — Task 1.
2. **UDS доступен чужому пользователю** (права сокета/каталога) — 0600/0700, тест. Тест — Task 7.
3. **UDP-объявление агента подделывается** (чужой VM в мосту) — announce несёт токен аплаенса; демон игнорирует несовпадающие. Тест — Task 10.
4. **GUI-форма создаёт невалидные профили** (клиентская валидация расходится с серверной) — форма гоняет те же правила через `bottle create` и показывает его ошибки дословно. Тест — Task 4.
5. **macOS-ветка ломает Windows-сборку** (cfg-протечки) — три таргет-гейта в каждом PR. Тест — Task 8.

---

### Task 1: CLI `--json`-режим

**Files:** Modify: `crates/cli/src/main.rs`, `crates/core/src/ipc.rs` (serde snake_case-аудит), тесты
**Interfaces:**
- Глобальный флаг `--json` (до подкоманды и после — оба положения): вывод = serde_json CliReply (list/show → свои reply-типы), ошибки — `{"Error":{"message":…}}` в stdout, коды 0/1/2.
- `--version --json` → `{"version":"…"}`.
- Golden-тесты: каждая команда в json-режиме против эталонной строки (сниппеты); тест «конфиг-содержимое не встречается ни в одном json-выводе» (show выводит профиль — поле config = имя файла, не содержимое — пин).

### Task 2: Tauri-каркас

**Files:** Create: `gui/` (workspace-внешний — собственный манифест Tauri-проекта: `gui/Cargo.toml`, `gui/src/main.rs`, `gui/ui/` (Vite+TS), `gui/tauri.conf.json`), CI-шаги
**Interfaces:**
- `gui` НЕ входит в cargo-workspace корня (Tauri приносит свою экосистему) — отдельная сборка `npm i && npx tauri build` в `gui/`.
- Rust-сторона минимальна: команды-посредники `run_bottle(args) -> {stdout, code}` (spawn, utf8, таймаут 130 с) — вся логика в TS.
- Dev-режим (`npx tauri dev`) работает на macOS-хосте против реального `bottle` из PATH.
- CI: джоба `gui-build` (windows + macos матрица): node + rust + `tauri build` → артефакт-бинарь; линтеры TS (eslint) в той же джобе.

### Task 3: GUI — список и управление

**Files:** Create: `gui/ui/src/app.ts`, `list.ts`
**Interfaces:**
- Экран: таблица бутылок (id/name/kind/state/socks5/forwards) из `bottle --json list` + `status` (merge по id), кнопки Старт/Стоп, поллинг 2 с, заблокированные кнопки во время переходов.
- Ошибки CLI показываются дословно (тост/строка); «демон не запущен» — отдельное состояние с инструкцией (Windows: bottled; macOS: .app сам поднимает? НЕТ — ruling D1: GUI только спавнит `bottle`; запуск демона = отдельная кнопка «Запустить демона» (spawn bottled, лог в окно) — единственное место, где GUI спавнит не-CLI).

### Task 4: GUI — create/edit и doctor

**Files:** Create: `gui/ui/src/create.ts`, `doctor.ts`
**Interfaces:**
- Форма create: поля как у `bottle create` (+ выбор файла конфига через системный диалог — путь передаётся в `--config`); submit = `bottle --json create …`; ошибки демона — красным под формой (Review Focus #4).
- Edit: `show --json` → форма (кроме id) → delete+create (`--force`)? РЕШЕНИЕ: edit = `create --force` с тем же id (M3-семантика) — одна операция.
- Doctor-панель: `bottle --json doctor` → таблица ✓/⚠/✗; кнопка «Собрать логи» → `logs export` → путь показан + кнопка «Открыть папку» (Tauri opener).

### Task 5: GUI — tray и полировка

**Files:** Create: `gui/ui/src/tray.ts`; Modify: tauri.conf
**Interfaces:**
- Tray-иконка: меню «Показать окно», список бутылок со старт/стоп (макс 8 + «…»), «Выход» (GUI выходит; демон живёт — документируем).
- Закрытие окна в tray (не выход); фича-флаг `--no-tray`.
- Ручной smoke-чеклист GUI (CI головы не проверяет) — в отчёт задачи.

### Task 6: GUI в релизную сборку

**Files:** Modify: `.github/workflows/ci.yml` (release-джоба: + gui-артефакты: `bottle-gui.exe` в MSI/ portable; `.app` — этап B)
**Interfaces:**
- Windows: `bottle-gui.exe` добавлен в WiX (ярлык меню Пуск) и portable-zip; версии синхронизированы.
- macOS-Windows-гейт: GUI-этап A завершён, Windows-релиз полон (M4a можно поставлять независимо от macOS).

### Task 7: Транспорт-трейт CLI↔демона (UDS на macOS)

**Files:** Modify: `crates/core/src/ipc.rs` (`pub trait CliChannel` / фабрика), `crates/daemon/src/pipe.rs` (+uds.rs), `crates/cli/src/main.rs`
**Interfaces:**
- Windows: named pipe (как сейчас). macOS: UDS `<data>/daemon.sock` (владелец-пользователь, 0600; каталог 0700 — уже есть у Store).
- Протокол поверх тот же LengthJsonCodec; retry-семантика CLI сохранена (ECONNREFUSED ≈ «демон не запущен»).
- Тесты: macOS-CI гоняет реальный UDS-круг (сервер+клиент в proc-тесте); права сокета запинены.

### Task 8: macOS-компиляция демона/CLI (env-only)

**Files:** Modify: cfg-гигиена в daemon (host_manager/svc_client/host_net/ps_query — за `#[cfg(windows)]` + не-windows заглушки с русскими ошибками), main
**Interfaces:**
- `cargo build -p daemon -p cli` на aarch64-apple-darwin зелёный; kind=host на macOS → «host-бутылки поддерживаются только на Windows» (D2); svc_pipe_exists=false.
- CI: macos-14 джоба — test --workspace (unit, UDS-тест) + check aarch64; три таргета зелёные в каждом PR (Review Focus #5).

### Task 9: SPIKE — vz-крейт (GO/NO-GO гейт этапа B)

**Files:** Create: `spikes/vz-poc/` (отдельный crate, не в workspace-релизе)
**Interfaces — критерии go (все обязательны):**
- `vz` компилируется на macOS arm64; бутстрап Linux-VM из arm64-rootfs (минимальный Alpine + ssh? НЕТ: наш rootfs с агентом); virtio-net NAT (vmnet shared).
- Агент внутри VM получает IP по DHCP и шлёт UDP-announce (токен) на broadcast; POC-хост ловит, коннектится control-TCP, проходит Hello/Up/Down полного цикла bottles (существующие agent-тесты можно прогнать внутри VM? хотя бы hello+up+down).
- Замеры: старт VM ≤15 с, память ≤512 МБ idle.
- Отчёт-решение: `vz` идёт / Swift-хелпер / отложить M4b — с фактами. Провал ANY критерия → эскалация владельцу, этап B не стартует.
- CI: спайк не в CI (VF недоступен на раннерах) — прогон на dev-машине, отчёт в репо.

### Task 10: macOS ApplianceManager (VzRunner)

**Files:** Create: `crates/daemon/src/vz_runner.rs`; Modify: appliance.rs (платформо-специфичная реализация за существующим WslRunner-трейтом — переименовать в `VmRunner`), appliance multi-arch
**Interfaces:**
- Жизненный цикл VM: rootfs → диск (образ-копия в `<data>/vm/`), старт при первом ensure, остановка при простое без бутылок ≥60 с (T11-обратный), keepalive при живых.
- Токен: провижинируется при первом создании диска (инъекция в rootfs до старта — файл в образ); announce-протокол с токеном (Review Focus #3: несовпадение → игнор).
- ensure-семантика та же: не запущена → старт → IP (announce, таймаут 30 с) → Hello.
- Тесты: фейк-VmRunner (unit, паттерны appliance-тестов M1); announce-парсер/валидатор — unit; реальные — ручной чеклист.
- `build-rootfs.sh aarch64` + CI-артефакт rootfs-aarch64.

### Task 11: macOS .app-бандл

**Files:** Create: `scripts/build-macos-app.sh` (macOS-раннер CI); Modify: release-джоба, tauri.conf (Resources)
**Interfaces:**
- Собирает: bottle/bottled (aarch64) + rootfs-aarch64 + GUI (Tauri .app) → Resources внутрь .app; zip + SHA256; GUI-кнопка «Запустить демона» спавнит bottled из Resources (пути относительно exe — D5).
- README: macOS-секция переписана (установка .app, env-only, ограничения D2); авто-восстановление работает (health-цикл общий).
- CI: macos-14 джоба релиза; install-smoke macOS: распаковка, `bottle --version`, `bottle doctor --json` (wsl-проверка на macOS не выполняется — doctor-платформо-осознанный).

### Task 12: Чеклист M4 + версии + финал

**Files:** Create: `docs/checklists/m4-verification.md`; Modify: README (статусы M4), версия → 0.3.0
**Interfaces:**
- Чеклист: (1) Windows: GUI из MSI — создать/старт/стоп/RDP-путь, tray, doctor, logs; (2) macOS: .app на чистой машине — демон из GUI, env-WG-бутылка стартует, SOCKS5-выход через туннель (curl --socks5 → внешний IP), сон/резюме → авто-восстановление; (3) UDP-announce: посторонний broadcast с неверным токеном игнорируется; (4) двойной старт GUI (single instance — Tauri плагин или лок-файл); (5) удаление .app = чистота (нет хвостов вне <data>).
- Все статус-таблицы README синхронизированы; локальные гейты зелёные.

## Что НЕ входит

- Host-бутылки на macOS (D2), редактор конфигов VPN в GUI, мультиязык/темы, подпись/нотаризация, автообновление GUI, дистрибуция DMG (zip достаточно; DMG — пост-релизная полировка), Linux-десктоп.
- Замена Tauri на нативный AppKit/SwiftUI — отвергнуто: один GUI-стек на обе ОС.
