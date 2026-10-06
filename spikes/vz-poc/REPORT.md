# vz-спайк (M4 Task 9) — отчёт-решение GO/NO-GO

Дата: 2026-10-06. Статус: **NO-GO для «vz идёт»; этап B (M4b) откладывается с эскалацией владельцу.** Swift-хелпер (fallback D3) НЕ решает причину — см. ниже.

## Что проверялось (критерии плана Task 9)

1. VM из arm64-rootfs с агентом, virtio-net NAT — **частично**: сама VM стартует.
2. DHCP + UDP-announce + control-TCP Hello/Up/Down — **не достигнуто**: guest-kernel не грузится до userspace.
3. Замеры (старт ≤15 с, память ≤512 МиБ) — **не измеримо** (guest не поднялся).

## Факты

### Работает

- **Биндинги Virtualization.framework**: `objc2-virtualization 0.3.2` (крейта с именем `vz` на crates.io НЕТ — план имел в виду VF-биндинги; ruling: objc2-virtualization — канонические автогенерированные биндинги семейства objc2). Компилируется на aarch64-apple-darwin, конфигурация VM (LinuxBootLoader, generic platform, virtio-net NAT, virtio-serial) валидируется `validateWithError`, `startWithCompletionHandler` возвращает успех, `VZVirtualMachineState::Running`.
- **Entitlement**: VF требует `com.apple.security.virtualization`; решается adhoc-подписью (`codesign --sign - --entitlements entitlements.plist`) без Apple Developer — то же понадобится bottled/.app в T11 (tauri-бандл подписывается с этим entitlement).
- **Сборка образа без docker** (dev-машина без Docker): Alpine 3.22 aarch64 minirootfs + пакеты прямой распаковкой .apk (resolve-apk-deps.py), настоящий `bottle-agent` собран локально `RUSTFLAGS="-C linker=rust-lld" cargo build --release -p agent --target aarch64-unknown-linux-musl` (rust-lld линкует статический musl-ELF без musl-gcc).
- Собственный cpio newc-архиватор (macOS cpio пишет odc) с узлами /dev для initramfs.

### Не работает: guest-kernel не доходит до userspace

Два ядра, стабильно `VZVirtualMachineState::Running`, но консоль (virtio-serial, `console=hvc0`) молчит, vCPU 0%:

| Ядро | virtio | HYPERV | Результат |
|---|---|---|---|
| Alpine 6.12.112 linux-virt (unzboot из PE-контейнера) | console=y, net/blk=**m** | **y** | В ~1 запуске из 5 одна строка `[0.059] hv_vmbus: registering driver hyperv_keyboard`, затем тишина и 0% CPU; обычно 0 строк |
| Ubuntu 6.8.0-71 generic (raw Image из gzip-vmlinuz) | **все =y** | m | 0 строк, 0% CPU |

Варьировались: CPU 1/2, память 512 МиБ, initrd gzip/несжатый, rdinit sh/init/отсутствует, ignore_loglevel. Точная реплика «удачного» запуска молчит — ранний стоп недетерминирован.

## Почему Swift-хелпер не спасает

Fallback D3 (вендорный Swift-хелпер) — это другой API-слой НАД ТЕМ ЖЕ Virtualization.framework. Проблема локализована ниже уровня биндингов: guest-kernel умирает в первые миллисекунды после старта vCPU. Хелпер на Swift даст тот же результат.

## Гипотезы (не проверены — вне таймбокса спайка)

- Требования к образу kernel от VZ, которым не удовлетворяют дистрибутивные ядра (layout/KASLR-seed/PSCI-детали Apple-гостя); успешные проекты (lima/crc/vfkit) используют собственные или специально собранные ядра.
- Взаимодействие `CONFIG_HYPERV=y` (Alpine) с Apple-гостевым ACPI/dtb — точка останова «hv_vmbus» подозрительна, но Ubuntu (HYPERV=m) молчит тоже.
- Дефект в нашей ранней конфигурации платформы (например, требование VZGenericPlatformConfiguration-полей: `machineIdentifier` и т.п.).

## Пути продолжения (для решения владельца)

1. **Отложить M4b** (рекомендуется сейчас): M4a (GUI Windows) полностью готов к поставке независимо.
2. Продолжить подбор kernel за рамками спайка: собрать mainline-ядро `defconfig` + virtio=y (в VM/Docker), либо взять kernel+initrd из проверенного VZ-проекта (vfkit/lima-ассеты) и повторить `run.sh`.
3. Запросить upstream (objc2-virtualization / Apple dev forums) про минимальный рабочий Linux-boot-рецепт на macOS 27 arm64.

## Воспроизведение

- `bash build-image.sh` — minirootfs+пакеты+агент → rootfs.cpio.gz; kernel: `Image` (Alpine, unzboot) / `Image-ubuntu`.
- `./run.sh` — подпись entitlement + запуск; ручки `POC_KERNEL=alpine|ubuntu`, `POC_INIT=sh|init`, `POC_CPUS=1|2`.
- Компоненты: `resolve-apk-deps.py` (apk-зависимости по APKINDEX), `mkcpio.py` (cpio newc), `entitlements.plist`.
