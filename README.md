# Pair-wise GSB

一个完全离线的 P/S 波拾取分配、二维震源定位与台站时钟偏差估计服务。实现包含 Rust HTTP 服务、SQLite 存储、确定性求解器、原生 HTML 可视化页面和可重放的自带 JSONL 数据集。

## 快速开始

```bash
cargo fetch --locked
cargo test --locked
cargo run --locked -- --listen 127.0.0.1:5312
```

浏览器访问：

```text
http://127.0.0.1:5312
```

可选参数：

```bash
cargo run --locked -- --listen 127.0.0.1:5312 \
  --database pairwise_gsb.sqlite \
  --demo data/demo.jsonl
```

首次启动且数据库为空时，服务导入 `data/demo.jsonl`，用不可变的 `clock-v1` 生成基线候选；之后重启不会重复导入或累计证据。

## 输入格式

输入为 JSONL，每行一个对象，字段名采用 snake_case。支持：

- `station`：台站 ID、平面 x/y、高程。
- `velocity_model`：P/S 速度和平面有效范围。
- `clock_correction`：按版本和台站分段的时钟偏差。
- `pick`：台站、P/S、到时、1σ 不确定度、置信度、来源和模型版本。

时间必须是 ISO-8601 且显式带 UTC 偏移，例如 `1970-01-01T02:00:01.043Z`。示例数据集包含 4 个台站、3 个合成事件、2 个时钟版本、1 个噪声拾取和 1 个不可能 S 先于 P 的拾取。

## 单位与数值约定

- 台站、震源坐标和距离：km。
- 深度：固定为 `0.0 km`；当前模型是二维平面定位，不伪造三维深度。
- 到时、发震时刻、残差、时钟偏差和速率：秒；时钟速率为 s/s。
- 速度：km/s。
- 拾取不确定度：1σ，单位秒，必须大于 0。
- 内部时间使用自 Unix epoch 的 64 位浮点秒数；API 与 JSONL 使用 ISO-8601 字符串。
- 输出残差和坐标四舍五入到 0.001；协方差元素保留到 1 ns² 或对应单位尺度，避免浮点尾差影响显示比较。
- 关联窗口：同一 P 波簇最大间隔 `30 s`，同一候选的 S 波在 P 簇种子后 `0–15 s`。
- 自动剔除阈值：到时残差绝对值超过 `4σ`；锁定拾取即使超阈值也保留，并通过警告显示矛盾。
- 网格初值步长：`0.5 km`。
- Gauss–Newton 最大迭代：10 次；Cholesky 对角元容差 `1e-10`，低于该值视为秩不足/非正定。

## 求解与约束

求解器按以下顺序执行：

1. 对非噪声 P 拾取按时间确定性分簇；少于两个不同台站的单点噪声不会成为事件种子，少于三个台站的候选保留但标记欠定。
2. 为每个事件生成完整台站候选和逐个移除台站的备选，至少保留每个事件前 3 个候选。
3. 在速度模型边界内做确定性网格初值搜索。
4. 使用加权 Gauss–Newton 拟合 x、y、发震时刻和相对参考站的时钟残差。
5. 对 4σ 外拾取执行一次保守重拟合；锁定项不允许被自动剔除。
6. 计算方位空档、RMS、逐拾取 χ² 贡献和正定协方差。
7. 多事件共享时钟残差只有在联合解严格降低总 χ² 时才接受，否则保留独立解并记录拒绝原因。

候选状态：

- `reliable`：满秩、通过正定性检查且无边界/欠定警告。
- `conditional`：有坐标结果，但存在边界、几何空档、自由度或时钟不可分辨等警告。
- `underdetermined`：台站/相位不足或 Hessian 非正定；不输出伪精确坐标。

每个候选显示：

- 接受和剔除拾取、残差、σ、χ² 贡献、锁定状态。
- 站点方位覆盖（最大空档角度）。
- 正定协方差矩阵和参数标签。
- 每类拒绝原因，例如 `arrival_residual_gt_4sigma`、`s_arrival_before_station_p`、`marked_as_noise`、`outside_association_window`。

## 确定性承诺

- 求解不使用线程、随机数、系统时间排序、HashMap 迭代顺序或网络数据。
- 所有枚举和聚合使用稳定 ID、`BTreeMap` 或显式排序。
- 同分候选按 `score -> event_key -> x -> y` 的全序稳定排列。
- 批次、运行、候选和假设 ID 由 SHA-256 内容哈希生成。
- 相同输入、相同二进制和相同 `Cargo.lock` 产生相同 JSON 快照；测试中对完整候选 JSON 做二次运行比较。
- 排序只依赖显式输入和稳定数值规则，不依赖调度顺序。

## 时钟版本与复现

`clock_segments` 是只插入、不可覆盖的版本化分段表。新版本不会隐式改变旧候选：

- 初始基线冻结在 `clock-v1`。
- 页面或 API 必须显式选择 `clock-v2` 和作用域（全部或指定事件）才会重算。
- 每次运行写入 `solver_runs` 和 `candidate_snapshots`，旧运行仍可通过 `/api/run?id=...` 完整查看。
- 当前状态按事件选择各自最新运行，因此只重算 `event-002` 时，其他事件仍显示旧版本结果；历史运行列表可切回完整旧快照。
- 冻结候选写入独立假设表；复制冻结候选会创建并行工作假设，不覆盖父快照。

## SQLite 结构

重要表：

- `import_batches` / `import_records`：内容寻址批次和原始记录。
- `stations`、`velocity_models`、`clock_segments`：基础参考数据；时钟分段按版本追加。
- `picks`：当前拾取状态；原始导入记录保存在 `import_records`。
- `solver_runs`：不可变求解运行参数。
- `candidate_snapshots` / `candidate_pick_snapshots`：不可变候选和逐拾取证据。
- `event_hypotheses`：冻结或并行假设。
- `operation_log`：追加式操作日志。

冻结候选、校正版本、操作日志没有混在一张可覆盖表里。当前指针和易变拾取状态与审计快照分离。

## 幂等导入和崩溃恢复

导入在单个 SQLite 事务中完成：

1. 对非空 JSONL 行做规范化 JSON 序列化。
2. 计算完整内容的 SHA-256。
3. 如果哈希已存在，直接返回同一 `batch_id`，不插入第二条记录。
4. 新内容先写批次，再逐条写参考数据和原始记录，最后写操作日志并提交。

进程在提交前崩溃不会留下部分批次；提交后重复导入同一文件只返回 `duplicated: true`，不会重复累计拾取证据。

## HTTP API

- `GET /api/state`：台站、模型、拾取、分段、当前每事件候选、运行和假设。
- `POST /api/import`：`{source_name, content}`。
- `POST /api/picks/status`：`{pick_id,status,locked_event}`，状态为 `active|locked|noise`。
- `POST /api/solve`：`{clock_version,scope,rationale}`，空 scope 表示全部事件。
- `POST /api/freeze`：`{candidate_id,label}`。
- `POST /api/hypotheses/duplicate`：`{hypothesis_id,label}`。
- `GET /api/run?id=<run_id>`：读取一次旧运行的完整候选。
- `GET /api/runs`、`GET /api/batches`、`GET /api/logs`。

## 已知边界

- 震源深度固定为 0；不包含外部地图、外部速度模型或远程目录。
- 速度模型为平面均质 P/S 模型，无外部地形投影。
- 页面用于本地审计和操作，不做用户认证；服务只应绑定可信本地地址。
