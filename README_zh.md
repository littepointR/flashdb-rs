# FlashDB-rs

[English](README.md) | 简体中文

从零用 **Rust 重写的 [FlashDB](https://github.com/armink/FlashDB)** —— 面向裸 Flash 的超轻量级嵌入式键值数据库（KVDB）与时序数据库（TSDB）。

本实现完整保留了经工程验证的 FlashDB 盘面格式（FlashDB-C 写入的数据可直接由 Rust 挂载，反之亦然），同时以 `no_std`、零堆分配（allocation-free）的原生 Rust 架构彻底重写了内部实现。在 `harness/` 基准套件上，它比原版 C 实现**速度更快、RAM 占用更低**；在启用 `size` 编译配置、`small-crc` 特性与 `-InlineThreshold 50` 编译参数时，它在真实 MCU 硬件上同时达成了 **ROM 与栈空间的全面追平甚至反超**，并在 NUCLEO-U545RE-Q 开发板上通过了全部 11/11 项 C vs Rust 对照验收测试（实测数据详见 [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md)，优化记录详见 [`docs/OPTIMIZATION.md`](docs/OPTIMIZATION.md)）。

```rust
use flashdb::{Config, Flash, Kvdb};

struct NorFlash { /* 你的芯片 / 分区 SPI 或片内 Flash 驱动 */ }
impl Flash for NorFlash {
    type Error = core::convert::Infallible;
    const WRITE_GRAN_BITS: u32 = 8;     // 1 = NOR, 8 = STM32F2/F4, 32 = STM32F1, 128 = STM32U5 ...
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), Self::Error> { unimplemented!() }
    fn write(&mut self, addr: u32, buf: &[u8]) -> Result<(), Self::Error> { unimplemented!() }
    fn erase(&mut self, addr: u32, len: u32) -> Result<(), Self::Error> { unimplemented!() }
}

# fn demo(flash: NorFlash) -> Result<(), flashdb::Error<core::convert::Infallible>> {
// 4 个 4 KiB 擦除块 = 一个 16 KiB 分区
let mut db = Kvdb::mount(flash, Config::new(4096, 16 * 1024))?;

db.set(b"boot_count", &42u32.to_le_bytes())?;

let mut value = [0u8; 16];
if let Some(n) = db.get(b"boot_count", &mut value)? {
    let count = u32::from_le_bytes(value[..n].try_into().unwrap());
    let _ = count;
}

let mut it = db.iter();
while let Some(entry) = it.next() {
    let entry = entry?;                       // 键名、长度、状态、地址
    let mut buf = [0u8; 64];
    let n = it.read_value(&entry, &mut buf)?; // 校验整条记录的 CRC32
    let _ = (&entry.name(), n);
}
# Ok(())
# }
```

## 与 C 原版的核心差异

本项目是架构重写，而非逐行翻译。盘面二进制格式完全兼容，但内部实现为纯正的 Rust 设计：

* **无全局状态。** 数据库通过泛型（`Kvdb<F>` / `Tsdb<F, T>`）直接持有 Flash 驱动，无需 FAL 分区注册表或全局锁。只需为目标分区实现 `Flash` trait（或使用内置的 `Partition<F>` 切片适配器划分同一物理 Flash 的多个子分区）并传入即可。
* **无堆内存分配。** `#![no_std]` 且零外部依赖，所有缓冲区均位于栈上或由调用方提供。库开启 `#![deny(unsafe_code)]`，仅保留两个经严格审计的原地初始化函数（`Kvdb::mount_into`、`Tsdb::mount_into`），使存放在 `static` 静态存储区中的数据库（MCU 常见模式）无需在栈上构造临时副本。
* **强类型错误。** `Error<E>` 原样携带底层 Flash 驱动自身的错误类型并指明失败阶段；没有 `FDB_ASSERT` 死循环，也不会丢失底层错误码。支持 `Config::not_formattable(true)`（未格式化或头部损坏时返回 `Error::NotFormatted` 而非自动全盘擦除）。
* **真迭代器替代回调函数。** KV 与 TS 遍历均为标准 `Iterator`；TS 支持正向、反向及按时间范围遍历。数据载荷按需读取（并提供 `read_value_at` / `read_blob_at` 分段窗口读取，在校验完整记录 CRC32 的同时仅拷贝所需切片），遍历跳过记录时不会产生无谓拷贝。
* **默认 KV 自动版本迁移。** `Kvdb::auto_update(version, defaults)` 对齐 C 版 `FDB_KV_AUTO_UPDATE`，通过 `__ver_num__` 记录在固件升级时自动补齐新增默认键。
* **编译期静态特化。** 写入粒度（`WRITE_GRAN_BITS`）是 Flash 驱动的关联常量；盘面布局表、状态编码与对齐缓冲尺寸在编译期按具体 Flash 类型静态折叠，零运行期开销。时间戳位宽通过类型参数选择（`Tsdb<F>` = 32 位，`Tsdb64<F>` = 64 位）。
* **更少的 Flash 读写流量。** 查找操作仅验证记录头并比对键名，不会对途经的每条记录读取并计算完整 Value 的 CRC；`get` 在向调用方缓冲区拷贝数据的*同时*流式校验 CRC，无需将 Value 读取两遍。
* **更低的 RAM 占用。** 2 路组相联 64 项键名缓存（含 LRU 共 516 B）替代了 C 版 512 B 键名缓存 *外加* 约 320 B 扇区缓存。在 Cortex-M3 上 `Kvdb` 结构体仅 560 B（C 版为 1064 B），`Tsdb` 仅 64 B（C 版为 160 B），且可通过 `KvdbSmallCache` / `KvdbNoCache` 进一步裁剪。
* **更少的 CPU 周期。** 直接计算下一条记录地址（无需按记录逐字重扫魔数）、O(1) 缓存探测替代 64 项线性扫描、遍历过程中免除全量 Value CRC 计算。

架构细节详见 [`docs/DESIGN.md`](docs/DESIGN.md)，实测性能与资源数据详见 [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md)。

## 仓库目录结构

| 路径 | 说明 |
|---|---|
| `crates/flashdb/` | 核心库：`no_std`，零外部依赖 |
| `crates/flashdb-sim/` | 带读写擦计数与故障注入的 RAM NOR 模拟器（用于宿主机测试与基准，同样为 `no_std`） |
| `crates/flashdb-bench/` | Rust 侧基准测试程序 + C/Rust 盘面镜像生成与校验工具 |
| `harness/c/` | C 参考测试套件（RAM FAL、基准程序、镜像工具、行为一致性校验） |
| `harness/firmware/` | Cortex-M3/QEMU 与 STM32U5 实板固件对比套件（C vs Rust）：`.text/.rodata/.data/.bss`、峰值栈水位、DWT 周期 |
| `harness/SCENARIOS.md` | C 与 Rust 共用的标准基准测试协议规范 |
| `reference/FlashDB` | 由脚本拉取的上游 FlashDB 源码（仅用于差分对拍测试，不属于本库代码） |

## 真实硬件实测（NUCLEO-U545RE-Q）

`harness/firmware/u545re/` 在 **NUCLEO-U545RE-Q**（STM32U545RE，Cortex-M33）开发板上直接运行本库：执行与 QEMU 固件完全相同的 KVDB+TSDB 负载，使用硬件 DWT 周期计数器计时，并通过 SWD 读取 RAM 结果缓冲区。对照验收负载使用 RAM NOR 模拟器进行 C/Rust 严格同台对比；另有 6 个专用固件直接驱动 **STM32U5 片内真实 Flash 控制器** 进行掉电故障、擦除中途复位、耐久度与长稳测试。详见 [`harness/firmware/u545re/README.md`](harness/firmware/u545re/README.md)。

实板验证结论：固件以 `rc=0` 完成，多次重复运行的两行 `RESULT` 输出字节级一致，所有业务校验和（`kv_checksum`、`kv_iter_checksum`、`ts_checksum`）与 QEMU Cortex-M3 基线完全吻合。C 参考实现拥有相同的板级后端，因此在完全相同的硬件上进行了苹果对苹果的同台实测，且 **Stable 与 Nightly 的 `size` 构建均 11/11 全项通过验收**：ROM **13 292 B（Stable）/ 13 292 B（Nightly）** vs C 13 360 B，峰值栈 1 080 B vs 1 240 B，静态 RAM 640 B vs 1 072 B，且所有业务阶段均比 C 更快（总 CPU 周期减少 −35.5%；追求极致速度的 `release` 配置总周期减少 −44.2%）。实板测试还覆盖了 **TSDB 滚动覆盖（rollover）**（超出容量追加 1 200 条记录，验证活动窗口、重挂载及 NOR 位安全）并证明 C 与 Rust 留下的 **Flash 分区镜像字节级完全一致**。烧录使用 **probe-rs** 直接编程 STM32U5，无需升级板载 ST-LINK 固件。

针对片内真实 Flash（仅操作 Bank 1 末尾 8 个 8 KiB 测试页 `0x0803_0000..0x0803_FFFF`）的 6 项专项实板测试：
1. **C 模型镜像一致性 (`run_flash_parity.ps1`)**：以 `WRITE_GRAN=128` 写入确定性 KV+TS 负载，片内真实 Flash 读回结果与宿主机模型 **字节级完全一致**；
2. **外部随机复位掉电注入 (`run_flash_fault.ps1`)**：50 次随机时刻硬件复位中断，**50/50** 轮完全恢复；
3. **扇区擦除中途复位注入 (`run_flash_erase_fault.ps1`)**：驻留于 SRAM 的钩子在硬件页擦除进行中（确认 `FLASH_NSSR.BSY == 1`）触发 `SYSRESETREQ` 系统复位，覆盖 20 轮（10 次 KVDB GC 页擦除 + 10 次 TSDB 滚动页擦除，通过 RAM 向量表拦截并处理 16 个半擦除页上的 121 次硬件 `FLASH_ECCR.ECCD` 双比特 ECC NMI 异常），**20/20** 轮完全恢复、零错误；
4. **20 次冷启动重挂载浸泡 (`run_flash_reboot_soak.ps1`)**：20 次启动 / 19 次硬件复位，验证跨重启持久化与重挂载一致性；
5. **240 秒片内 Flash 擦写耐久度 (`run_flash_tests.ps1 -Test endurance`)**：在全部 8 个测试页上完成 **22 493 次 P/E 循环**（编程 740 967 个 quad-word，22 509 次页擦除，0 错误）；
6. **60 秒 KVDB+TSDB 长稳压力 (`run_flash_tests.ps1 -Test longrun`)**：完成 72 轮业务循环、1 405 次扇区擦除及 18+18 次校验重挂载（0 错误）。

完整数据表格、复现命令与验收配置详见 [`harness/firmware/u545re/README.md`](harness/firmware/u545re/README.md)；测试清单与覆盖率（行覆盖率 **94.45%**，函数覆盖率 **99.54%**）详见 [`docs/TESTING.md`](docs/TESTING.md)。

## 格式兼容性

在相同的扇区大小、写入粒度与时间戳位宽下，盘面二进制格式与 FlashDB 2.2.x 字节级兼容：

* Rust `Kvdb`/`Tsdb` 可直接挂载由 FlashDB-C 写入的分区镜像；
* FlashDB-C 可直接挂载由本库写入的分区镜像；
* 确定性写入负载生成的 **Flash 镜像字节级完全一致**（由 `scripts/rust_compat.sh` 自动验证，见 `docs/BENCHMARKS.md`）。

支持的写入粒度（`WRITE_GRAN_BITS`）：1（位级 NOR）、8（字节编程）、32、64、128、256 位（覆盖主流外部 NOR 与各系列 STM32 片内 Flash）。仅支持小端（Little-endian）架构。未移植文件系统模式（`FDB_USING_FILE_*`）；如需在宿主机文件上运行，为文件封装 `Flash` trait 即可。已知有意差异详见 `docs/DESIGN.md` 中的 “Known deviations”。

## 构建与测试

前置依赖：Rust 工具链（`cargo`）、C99 编译器、用于固件对比的 `arm-none-eabi-*` 与 QEMU，以及用于差分测试的上游 FlashDB 源码：

```sh
scripts/fetch_reference.sh               # 将 FlashDB 2.2.x 克隆至 reference/FlashDB
cargo test --workspace --release         # 宿主机全套单元测试、集成测试、C 镜像对拍与随机状态机模型测试
cargo test -p flashdb --release --test long_run -- --ignored   # 长时/大数据量浸泡测试
cargo doc --workspace --no-deps          # 生成 API 文档 -> target/doc/flashdb
cargo build -p flashdb --target thumbv7m-none-eabi --release   # 裸机 no_std 编译检查

scripts/c_bench.sh                       # 运行 C 参考基准测试 -> harness/results
cargo run -p flashdb-bench --release --bin flashdb-bench -- --wg 8 --runs 5 --with-ts
python3 scripts/compare_bench.py --wg 8  # C vs Rust 逐项性能对比与验收检查

scripts/rust_compat.sh                   # C <-> Rust 全写粒度盘面镜像双向兼容矩阵
scripts/c_test.sh                        # C 参考行为测试
scripts/fw_c.sh && scripts/fw_rust.sh    # 固件 ROM/RAM/栈占用对比 (QEMU)
```

在真实硬件（`NUCLEO-U545RE-Q`，详见 [`harness/firmware/u545re/README.md`](harness/firmware/u545re/README.md)）上运行：

```sh
harness/firmware/u545re/run_rust.sh       # POSIX: 编译 + 烧录 + 运行 (probe-rs/pyOCD/OpenOCD)
harness/firmware/u545re/run_rust.ps1      # Windows/PowerShell: 编译 + 烧录 + 运行 (probe-rs)
harness/firmware/u545re/compare.ps1 -RustProfile size -RustFeatures small-crc  # C vs Rust 11/11 项实板验收
harness/firmware/u545re/run_flash_parity.ps1       # 真实片内 Flash: C 模型镜像一致性
harness/firmware/u545re/run_flash_fault.ps1        # 真实片内 Flash: 外部复位掉电注入
harness/firmware/u545re/run_flash_erase_fault.ps1  # 真实片内 Flash: 确定性扇区擦除中途复位注入
harness/firmware/u545re/run_flash_reboot_soak.ps1  # 真实片内 Flash: 20 次重启浸泡
harness/firmware/u545re/run_flash_tests.ps1 -Test endurance   # 真实片内 Flash: 240 秒 P/E 耐久度
harness/firmware/u545re/run_flash_tests.ps1 -Test longrun     # 真实片内 Flash: 60 秒数据库长稳测试
```

基准测试使用具备严格 NOR 语义（编程仅清零位、擦除置 `0xFF`）的 RAM 模拟器，并精确统计每次 Flash 读/写/擦操作次数与字节数，确保跨机器结果可复现且真实反映 Flash 负载。

## 实测对比数据摘要

在相同负载与计数型 NOR 模拟器下与 FlashDB-C 对比；完整表格与测试方法见 [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md)。

| 指标（Cortex-M3 QEMU，相同负载） | C FlashDB | Rust |
|---|---:|---:|
| ROM（`-Os` vs `-z` + `small-crc` + `-InlineThreshold 50`） | 11 408 B | 11 632 B (+2.0 %)；**STM32U5 实板为 13 292 B vs C 13 360 B（反超 −68 B）** |
| 静态 RAM | 1 064 B | **624 B**（实板 **640 B vs C 1 072 B**） |
| 峰值栈水位 | 1 192 B | **1 008 B**（实板 **1 080 B vs C 1 240 B**） |
| 静态 + 栈总 RAM | 2 256 B | **1 632 B**（实板 **1 720 B vs C 2 312 B**） |
| KV Flash 读取次数（热读 / 未命中 / 更新 / 遍历，每次操作） | 6 000 / 159 / 11.1 / 15.9 | **4 / 37 / 8.0 / 6.9** |
| TS Flash 读 / 写 / 擦次数 | 所有场景完全一致 | 完全一致 |

两个对体积/速度权衡至关重要的构建选项：
* `--features small-crc`：使用 64 字节半字节 CRC32 查表替代默认的 1 KiB 字节表，节省 960 B `.rodata`（CRC 计算开销约 2 倍，仅对大 Value 有轻微影响）；
* `-C llvm-args=-inline-threshold=50`：修复 `-Oz` 默认内联阈值过于保守的问题，在实板上同时减少 84 B ROM 并降低 22% TS 追加 / 21% TS 滚动覆盖的 CPU 周期。

## 许可证

您可以任选以下许可证之一使用本项目：

* Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
* MIT license ([LICENSE-MIT](LICENSE-MIT))

本库是对 FlashDB 盘面格式与外部行为的独立 Rust 重新实现。仓库本身不包含上游 FlashDB 源码（https://github.com/armink/FlashDB，Copyright (c) 2020 Armink，Apache-2.0），仅在差分测试时通过 `scripts/fetch_reference.sh` 拉取。署名与来源说明详见 [NOTICE](NOTICE)。
