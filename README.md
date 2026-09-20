# Pair-wise GSB 离线拾取分配与震源定位

一个无外部地图、无地震目录、无网络依赖的本地 Rust 服务。系统导入可重放 JSONL，把 P/S 拾取软分配到一个或多个候选事件，同时估计震源位置、发震时刻和相对台站时钟偏差。冻结候选集、版本化时钟校正、锁定/噪声操作和审计日志分别存储为追加式 SQLite 记录。

## 快速开始

```bash
cargo fetch --locked
cargo test --locked
cargo run --locked -- --listen 127.0.0.1:5312
```

然后访问：

```text
http://127.0.0.1:5312
```

默认数据库为 `data/local.sqlite`，首次启动自动导入 `data/sample.jsonl`。也可以指定：

```bash
cargo run --locked -- --listen 127.0.0.1:5312 --db /tmp/pairwise-gsb.sqlite
```

## 单位与坐标约定

- 输入 `time`：内部统一为秒。数字按 POSIX 风格连续秒处理；也接受 `YYYY-MM-DDTHH:MM:SS.sssZ`，转换为 UTC 连续秒。
- 拾取不确定度：秒，合法范围 `[0.001, 60]`。
- 高程、深度、水平距离：米。
- 台站纬度/经度：十进制度。求解器把台站投影到以网络平均经纬度为原点的局部等距矩形平面（米），结果同时返回经纬度和局部 x/y。
- 速度：米/秒，模型层必须连续且满足 `0 < Vs < Vp`。
- 时钟分段值 `offset_s`：台站时钟相对真实时间的快偏量；计算到时使用 `corrected_time = raw_time - offset_s`。

## JSONL 输入

每行一个 JSON 对象，支持：

```json
{"record_type":"station","station_id":"ST01","latitude":35.70,"longitude":1.10,"elevation_m":120}
{"record_type":"velocity_model","version":"vm-small-1","layers":[{"depth_top_m":0,"depth_bottom_m":5000,"vp_m_s":5500,"vs_m_s":3180}]}
{"record_type":"pick","observation_id":"A-P","station_id":"ST01","phase":"P","time":1.324,"time_uncertainty_s":0.08,"confidence":0.95,"source":"auto","velocity_model_version":"vm-small-1"}
```

人工复核可以引用自动拾取；被引用的自动记录保留在数据库中，但求解器只使用人工版本：

```json
{"record_type":"pick","observation_id":"A-P-MANUAL","related_observation_id":"A-P","source":"manual", "...":"..."}
```

`data/sample.jsonl` 自带两个合成震源时窗、5 台支持的候选、人工复核记录以及一个不能形成多台事件的孤立低置信度拾取。

## 求解规则

- 一个拾取可以暂时支持同一时窗内多个候选；页面和 API 显示每个候选的到时残差、权重、得分贡献、台站方位覆盖空隙。
- 同站 P/S 在 `0.5 s` 以内、S 早于 P 或同相位间隔不可能时，会被剔除并给出原因。
- 少于两个独立台站的聚类不会伪造事件；其拾取进入 rejected 列表。
- 少于三个台站、加权 Jacobian 源参数秩不足、后验协方差非正定或条件数过大时，不输出精确坐标，只给出 `underdetermined` 候选与原因。
- 源坐标可识别但“每台时钟偏差 + 发震时刻”不能完全分开时，候选标为 `degraded`，保留估计值、秩、特征值和参数相关警告。
- 每个事件至少保留三个最优候选；候选按 `(状态优先级, 分数降序, RMS 升序, x, y, depth, origin_time)` 稳定排序。
- 所有记录排序使用 `BTreeMap`/`BTreeSet` 和显式数值比较；求解器不启动线程、不使用 HashMap 驱动排序，因此同分结果不依赖线程调度或哈希迭代顺序。
- 深度超出所有层 `[top, bottom]` 1 m 容差时拒绝该走时；数值比较使用 `1e-9 s` 时间容差。
- 走时模型为当前演示的小数据集使用直射线与分层常数速度；它不是区域生产速度模型。

## 锁定、噪声和并行假设

数据库初始化两个并行假设：`H-A` 与 `H-B`。

- 锁定拾取只对当前假设生效，重算时该拾取强制保留在原事件候选中；其他假设仍可改变。
- 标记噪声是全局证据动作；若拾取在任一假设中被锁定，系统拒绝标记，要求先全部解锁，避免静默破坏证据。
- 锁定或噪声后的重算会冻结新的 `candidate_sets` 行，旧集合从不覆盖。
- 页面可切换两个假设并读取任意历史候选集。

## 版本化时钟校正

`v1` 是所有台站、全时间段的零偏移。新版本由 `(version, station_id, ordinal)` 下的不可变分段组成。新版本不会自动影响任何候选：

1. 创建 `v2-*`；
2. 对某个假设和指定事件显式调用重算；
3. 新候选集记录自己的 `correction_version`；
4. 旧假设、旧事件和旧候选集仍可用原校正完整回放。

## SQLite 记录设计

不同生命周期的数据不混在一张可覆盖表里：

- `import_batches`：SHA-256 内容哈希、字节数、导入时间。
- `stations`、`velocity_models`、`picks`：原始输入证据，人工引用保留被替代自动拾取。
- `correction_versions`、`clock_segments`：不可变校正版本和分段函数。
- `hypotheses`：只保存当前候选集指针和当前选择的校正版本。
- `candidate_sets`：追加式冻结候选 JSON、父集合、校正版本、输入摘要哈希。
- `locked_picks`、`noise_picks`：用户约束。
- `operation_log`：操作载荷和内容哈希，独立审计追加日志。

崩溃恢复依赖导入批次哈希。进程在导入事务提交前退出不会累计证据；提交后以同一字节内容再次导入会返回 `duplicate=true`，不会重复插入台站、模型或拾取。

## HTTP API

- `GET /`：可视化页面。
- `GET /api/state`：台站、模型、拾取、校正、两假设、锁定、当前候选和日志。
- `GET /api/sets`：所有冻结候选集索引。
- `GET /api/set?id=N`：读取历史候选集。
- `POST /api/import`：体为 `{ "jsonl": "..." }`，按内容哈希幂等导入。
- `POST /api/lock`、`/api/unlock`：按假设锁定/解锁拾取。
- `POST /api/noise`：全局标记或取消噪声。
- `POST /api/corrections`：创建不可变分段校正版本。
- `POST /api/recompute`：可指定 `hypothesis_id`、`correction_version`、`event_key`；只冻结新增集合，不覆盖旧集合。

## 数值容差摘要

- 时间比较：`1e-9 s`。
- P/S 物理最小间隔：`0.5 s`。
- 同事件全局聚类时窗：`12 s`；同站只连接间隔 `[0.5, 60] s` 的 P-S。
- 软支持残差：`2.0 s`；硬拒绝残差：`4.0 s`，被锁定拾取例外但会产生矛盾警告。
- 速度模型深度边界容差：`1 m`。
- 协方差：报告秩、最小/最大特征值、条件数、源参数 1σ；使用弱先验保证数值矩阵可反演，并明确显示秩不足和条件数警告。

## 可复现性承诺

在相同 CPU 架构、同一 Rust 工具链、同一 `Cargo.lock`、同一输入字节和同一数据库版本下，候选排序、JSON 字段顺序和冻结集内容是可重复的。浮点结合顺序固定，代码不启动求解线程，也不依赖网络时间、外部地图或远程目录。跨不同 CPU/编译目标不承诺逐位浮点相同，但承诺相同的排序规则、集合选择和容差语义。
