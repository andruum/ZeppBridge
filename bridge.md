> **分支约定：本文档中的所有修复都将落在 `fix-markdown` 分支上；全部修完后由用户决定合并。本轮范围：B3、C1、A7。**

# ZeppBridge 路线图（基于 2026-09-11 全量审计的复核版）

> **Agent 交接规则：所有 Agent 按照本文档修复问题时都必须严格遵守——每修完一个待修项，就立即将该项前面的 `[ ]` 改为 `[x]`，以便后续 Agent 准确接续。**

> 复核方式：对原清单逐条只读对照当前源码 + `git show 7e2227f` + 未提交改动，未跑 build/test。
> 行号基线：commit `818c9bd`（原清单行号按更早期代码，已漂移，全部重定位）。
> 复核统计：P1 62 条——57 确认、3 部分、0 证伪；P2 ~126 子项——117 确认、6 部分、1 证伪、2 已被今日提交修复；P3 中 10 条被低估、建议升级。
> 总评：原清单的事实判断相当准，几乎没有读错代码的条目；偏差主要在**威胁模型与触发前提**——它把「机制存在」一律写成「后果会发生」，个别条目的数量级夸大（P2#46 的「数百万 chunk」实际 ~20 万）、或把 dead-code 埋雷当现洞（#29、51c/d）。按后果 × 触发面重定级后，本文件把散在 P1/P2/P3/勘误四处重复的同一问题合并成按主题组织的待办。

## 本轮验证（2026-09-12）

仅完成 B3、C1、A7。Rust workspace 测试 407 项通过、1 项真实旧库测试按原设置忽略；最终存储回归 117 项通过。cargo check/clippy、前端构建、126 项前端测试和 i18n 检查通过。未进行真实账号失败/取消实测、桌面 EXE 打包验收或发布；这些自动化结果不代表已经完成设备端验证。其余条目保持待办。

## 0. 先修的十件事

1. [ ] **回放管线三连**（A1）— 吞错盖章让坏 raw 永久跳过、单条坏报文让每次启动重放、归一化失败连坐删 raw — 重放失败要计数、可见、不推进 revision；raw 与归一化解耦事务。
2. [x] **首次同步失败永久失去补拉**（B3）— 新装用户第一次同步失败/取消后再也不会自动补 180 天 — 只给「成功有数据」的结果写 `last_cloud_sync_at`。
3. [ ] **panic 家族**（A2）— 一条云端怪值把回放/补拉线程整个打崩 — 时间算术统一改 `checked_*`。
4. [ ] **null→0 家族**（D1）— 缺 REM 画成「0 分钟 + 假色条」、未知断言成 0 — null 直通 `—`/`未提供`。
5. [ ] **假成功聚合**（B1）— 明细全败报「没有待拉取」、单条 Failed 折进 Success、支流失败仍删旧数据 — 聚合状态按真实结果分级。
6. [ ] **认证变更不取消旧同步**（B7）— 换账号后旧凭据继续写库且不可取消 — save/clear 先 `request_cancel` 再换 manager。
7. [x] **迁移降级防护**（C1）— 新 schema 库被旧构建静默降级盖章、无备份 — `version > CURRENT` 直接拒绝。
8. [ ] **MSI 误判 portable**（E1）— 安装版更新后在 LocalAppData 装出第二份 — 按安装方式而非路径猜测判断。
9. [x] **FIT local_timestamp 伪造时区**（A7）— 每次导出都向导入方断言「本地=UTC」— 读不出偏移就别写该字段。
10. [ ] **ExtractedLogin Debug 带明文 token**（F1）— 一次 `{:?}` 就把 token 落日志 — 手写 Debug 脱敏。

## A. 数据正确性与「不编造」

### [ ] A1 回放管线三连（原 #1 #2 #3，含勘误）
- 级别：P1
- 位置：`storage/mod.rs:2285-2325`（吞错+盖章）、`:2289-2294`（`?` 中止回放）、`:5682-5704`（连坐回滚 raw）、`sync/mod.rs:761-762`（「raw 已保留」假注释）
- 为什么值得修：一条坏报文有三种失败放大方式——被吞后 revision 仍推进（detail 先于摘要时明细永久跳过、派生行已被同事务 DELETE）；解码 `?` 让整轮回放中止且每次启动重跑；归一化失败连坐删掉已拿到的 raw，明细陷入「永远 pending、每次同步重拉」。
- 触发条件：云端返回一条 normalizer 不认识的确定性坏报文，正常使用即发生。
- 方向：失败计数+隔离（坏 raw 记 quarantine 行、不推进 revision）；raw 落库与归一化拆事务；删除假注释。
- 验收门：回放测试——一条坏 raw + 一条好 raw，断言好 raw 派生行在、坏 raw 计入失败且不阻断、revision 未推进；再断言失败 raw 本身仍在库里。

### [ ] A2 panic 家族（原 #14 + P3 升级 ×3）
- 级别：P1
- 位置：`decoder/workout_detail.rs:562-570`、`normalizer/mod.rs:730-731`、`:406-407`、`storage/mod.rs:6006-6009`、`insight/mod.rs:524`（start_time 近 MIN）、`sync/mod.rs:537`（`chunk_start[..7]` 无长度校验）、`storage/mod.rs:5942`（zlib 解压无上限）
- 为什么值得修：#1 的 `if let Ok` 只吞 Err 不吞 panic——一条带超大时间戳的 raw 直接炸掉回放/补拉线程；`"0001-01-01"` 这类日期经 IPC 传入即可下溢 panic；库里一条被篡改的压缩行可展开成任意大小。
- 触发条件：云端报文带极端时间值，或用户手填边界日期。
- 方向：时间算术统一 `checked_add/sub`，越界记 unverified 而非 panic；字符串切片先查长度；解压设输出上限。
- 验收门：fuzz 式单测——`start=i64::MAX`、`offset_ms` 极大、`"0001-01-01"` 输入不 panic 且产出 unverified/skip。

### [ ] A3 轨迹解码与导出边界（原 #10 #11 #12 + P2#93 + 54a/c + P3 currentDistance）
- 级别：P1
- 位置：`decoder/workout_detail.rs:668-697`（坐标无校验）、`:672-696`（跳点丢 delta）、`:799-807`（filter_map 错位）、`:662/674/736/1041`（i64 裸加）、`:713-716`（HR 样本无界）、`export_formats.rs:248-249`、`export_fit.rs`（纬度无 ±90 界）；currentDistance 坏点 → ~21k 个 0 时长 split
- 为什么值得修：坐标是累积差分——丢一个 delta 后整条轨迹系统性偏移且形状仍正常（最隐蔽的假数据）；解析失败静默压缩让心率/海拔按错索引配对；导出口是最后一道边界却把 lat=999 原样写进 GPX/FIT。
- 触发条件：云端 detail 报文含异常或缺失增量。
- 方向：差分解码失败就标坏点而非跳过；入库/导出双侧做坐标域校验；HR 样本沿用 lap 的 0..=250 界。
- 验收门：构造缺 delta 的 detail 报文，断言轨迹含 null/坏点标记而非平移。

### [ ] A4 归一化时间与缺失值（原 #13 + 53a–d + #6）
- 级别：P1
- 位置：`normalizer/mod.rs:703-706`（tz 静默当 UTC）、`:1330-1336`（NaN/inf 过闸）、`:1371-1380`（epoch 按 UTC 定日、无视 timeZone 字段）、`:472-477/524-533`（date-only 排序恒输被覆盖）、`:1760-1762`（weight 时间戳无上界）、`:1258-1276`（first_string 遮蔽回退 → 双行）、`:73/:417`（0 当真读数）；`fetcher/mod.rs:1146-1159`（spo2 7 天 ×288/天 >1000 条截断无声）
- 为什么值得修：这一组全是「把缺失/异常变成看起来正常的数字」——错时区归日、NaN 落 0、双行、未来 5.5 万年的体重、被截断的最旧 spo2 静默丢。
- 触发条件：云端报文缺 tz/阶段字段或带怪值；密集采血氧的账号。
- 方向：tz 缺失记 unverified；`parse_number` 先 `is_finite`；定日优先 payload 的 timeZone；spo2 chunk 缩短或加截断检测；0 值按缺失处理。
- 验收门：归一化测试——无 tz 报文产出 unverified 而非错日；`"NaN"` 字段入不了库。

### [ ] A5 睡眠缺失的 schema 表达（#13 补充，需迁移 v24）
- 级别：P1
- 位置：`migrations.rs:263` 附近——仅 `rem_available`；`normalizer/mod.rs:642-665`（缺 dp/lt/wk 落真 0，`duration=整段-awake`）
- 为什么值得修：schema 只有 REM 一列能表达「未提供」，deep/light/awake 被 NOT NULL 逼着填 0——后端自己都在 `rem_available` 上立了先例，其余三列跟上才能根治 D1 的显示侧。
- 触发条件：扁平 sleep 记录无阶段字段（老旧固件常态）。
- 方向：v24 迁移加 `deep_available`/`light_available`/`awake_available`（或列可空），normalizer 落 null。
- 验收门：无阶段字段的睡眠报文入库后三列为 null，前端显示 `—`。

### [ ] A6 周报与洞察错算（原 #9 + 55a–d；55e 中文 unit 并入 D6）
- 级别：P1
- 位置：`insight/mod.rs:711-759`（weekly 比较永不发生）、`:572`（UTC 今日 vs 本地日）、`:804-811`（双 scope 计两天）、`:643/:885`（零基线误报 thin_baseline）、`:960-968`（EPSILON 阈值）、`:767-788`（样本条数当天数）、`:580`（hrv/hrv_rmssd 名称分叉致误报无数据）
- 为什么值得修：周报是用户每周必读的产出，目前「比较永不发生还报理由」「4 天数据过 7 天门」「任何噪声都成趋势」三条让它系统性地讲假话。
- 触发条件：正常使用即触发（数据跨 scope/非 UTC 时区）。
- 方向：samples 与天数判据统一按「去重日期数」；阈值换有意义的 epsilon；修指标名分叉。
- 验收门：双 scope 同日数据下 baseline_count 只计 1 天；比较未发生时 fact 明确标 insufficient 而非编 reason。

### [x] A7 FIT local_timestamp 伪造时区（原 #51）
- 2026-09-12 修复：UTC 归一化后的 Z/+00:00 无法证明活动本地时区，因此省略 local_timestamp；仍保留显式非零偏移。FIT 文件生成后再解码，验证未知偏移字段缺失、UTC timestamp 保留，以及原有 +08:00 用例通过。
- 级别：P1
- 位置：`export_fit.rs:265-270` + `:1264-1271`（`local_offset_seconds` 对 `+00:00` 恒返回 0）；`types.rs:216`（start_time 是 `DateTime<Utc>`）
- 为什么值得修：入库即丢本地偏移，导出端却恒写 local_timestamp 字段，向所有导入方断言「本地时间=UTC」；:256-264 的注释自述「读不出就不写」，实现恰好违反自己的规则。
- 触发条件：任何 FIT 导出。
- 方向：读不出偏移就不写该字段（或入库侧补存时区）。
- 验收门：导出断言——本地偏移不可知时 FIT 里无 local_timestamp 字段。

### [ ] A8 覆盖账本把盲区染成「完整」（原 #5 #52 + 52a/f + P3 升级）
- 级别：P1
- 位置：`fetcher/mod.rs:184/357/498`（窗口内 404 子片跳过）、`sync/mod.rs:604-625`（`records_written==0` → EmptyFromCloud）、`fetcher/mod.rs:441-443`（中段 Unavailable 当结束）、`:559/:591`（可选子端点拖垮整流）、`:1043/:1063`（探针默认 error 且取消也落库存 7 天）
- 为什么值得修：账本是「哪里没数据」的唯一权威，现在 404 空洞、解析失败、被取消的探测全被记成「已确认为空/完整」且不再重试——用户看到「数据完整」时可能是「没拿到」。
- 触发条件：服务端对某 7 天片 404、报文形状 normalizer 不认识、同步中途取消。
- 方向：`records_written==0` 区分 Unverified 与真空；探针失败不持久化为 7 天 error；子片失败记账本重试。
- 验收门：Unverified 报文进账本为 unverified 而非 complete，下一轮补拉会重试。

### [ ] A9 UTC/本地日混用（P2#43、P2#39 的 cutoff 部分）
- 级别：P2
- 位置：`storage/mod.rs:1749-1754`、`:1824-1828`、`:5632-5634`（cleanup cutoff 用 UTC 日）；#9 的 `today` 问题已在 A6
- 为什么值得修：相邻函数用 `Local::now()`，这几处用 `Utc::now()`/`substr(ts,1,10)`（UTC 前缀）对 `daily_metrics.date`（本地日），边界日偏一天。
- 触发条件：UTC±大偏移时区的边界日。
- 方向：统一按本地日参与 date 列比较。

### A10 其它归一化/解码边角（一行一条，P3）
- [ ] `fetcher/mod.rs:220-225`：HR 空页再发一次无分页请求，其 `?` 可把成功空集变失败。
- [ ] `fetcher/mod.rs:227`：存的 "raw" 是重拼 `{"items":merged}`，非服务端原文（影响回放/审计口径）。
- [ ] `fetcher/mod.rs:44-46`：`end_day()` 把排他 end_utc 含当日，日粒度端点双拉边界日。
- [ ] `decoder/workout_detail.rs:744-751`：kilo_pace 兜底拆分不与 summary 对账（laps 已有对账先例）。
- [ ] `normalizer/mod.rs:1258-1276`：first_value/first_string 遮蔽回退（并入 A4 的 53c）。
- [ ] `fetcher/mod.rs:1043/:1063`：失败/取消探针存成 error 并持久化（已升级进 A8，此处留档）。
- [ ] `normalizer/mod.rs:1294`：`parse_heart_range` 用 `split(';').filter(...).enumerate()`，空段会移位 zone 索引。

## B. 同步状态机与结果诚实

### [ ] B1 假成功聚合（原 #7 #8 + P2#44 #45）
- 级别：P1
- 位置：`sync/mod.rs:648-681/339-349`（明细全败报 Success「没有待拉取」）、`:699-719`（任一成功即聚合 Success）、`:441-448`（外周流失败仍跑 retention 且 cleanup 错顶掉成功报告）、`:338-349`（空 pending 不清旧 failed）
- 为什么值得修：同步报告是用户判断「昨晚数据同步好了没」的依据，目前「全败」「部分败」「旧失败残留」三种都被显示成成功或永久陈旧。
- 触发条件：明细接口持续 404、支流失败 + retention 到期、上轮失败后本轮无 pending。
- 方向：聚合按「有无 Failed」分级；cleanup 错误与同步结果解耦；空 pending 也清 sync_state。
- 验收门：模拟明细流全 404，断言状态为 failed 且不计入 success。

### [ ] B2 取消语义（原 #15 #4）
- 级别：P1
- 位置：`sync/mod.rs:300-310`（取消映成 ConfigError→红条「配置有问题」）、`:270`（等锁窗口内取消同病，最长达 20s）、`fetcher/mod.rs:1234/1322-1323`（Cancelled/NeedsReauth 被死 guard 吞）
- 为什么值得修：用户点取消看到红色「配置有问题」是双重说谎；等锁期间取消要等满 20s 才被误报。
- 触发条件：同步等写锁期间或流边界点取消。
- 方向：专用 Cancelled 变体贯通链路；fetcher 两分支先判 is_cancelled。
- 验收门：等锁期间取消，断言 UI 出「已取消」而非错误红条。

### [x] B3 首次同步失败永久失去 180 天补拉（原 #24）
- 2026-09-12 修复：失败/取消及首次零记录不写成功时间，保留失败结果；启动迁移不再从失败残留 raw 反推成功；首次补拉仅在取得记录后调度。回归覆盖失败、取消、空结果、迁移重入、成功后失败保留原时间。
- 级别：P1
- 位置：`commands/sync.rs:249/266`（failed/cancelled 也写 `last_cloud_sync_at`）、`useSyncController.ts:356/397-404`（wasFirstSync 只查存在性）、`:531`（launchSyncIsDue 同被骗）
- 为什么值得修：新装用户第一次同步失败（网络抖一下即够），自动补拉与启动同步判定永久失效，无任何自愈路径——后果比原描述更重。
- 触发条件：新装首同步失败或取消。
- 方向：只有成功且有结果时才写 last_cloud_sync_at。
- 验收门：模拟首次同步失败，断言 next launch 仍调度 first-run backfill。

### [ ] B4 deferred / Busy 礼让（原 #20 #21）
- 级别：P2
- 位置：`commands/sync.rs:198`（先查旗）、`lib.rs:364-369` vs `storage/mod.rs:2242`（立旗在拿锁后）、`:5549`（compaction 同构）、`commands/sync.rs:93-126`（补拉无 deferred 分支）
- 为什么值得修：升级后回放/压缩窗口内同步，等 20s 后被记成 failed + `err.core.busy` 红条，无自动重试。
- 触发条件：大库升级后回放期手动/自动同步或补拉。
- 方向：立旗先于拿锁；Busy 映射为可重试而非 failed。

### [ ] B5 明细积压无 deadline 无 LIMIT（原 #16）
- 级别：P2
- 位置：`sync/mod.rs:648-681`、`storage/mod.rs:2819-2839`（无 LIMIT/attempts）
- 为什么值得修：老账号首同步数百条明细逐条拉，界面长时间停「正在同步跑步明细」；永久失败条目每次重试。
- 触发条件：首次同步大积压或明细接口持续失败。
- 方向：循环查 deadline；pending 查询加 LIMIT + attempts 衰减。

### [ ] B6 补拉的停止与循环（原 #23 + P2#109 #119 #72 #79）
- 级别：P2
- 位置：`HistoryArchivePanel.vue:417-419/439-469`（停止只置本地旗、循环不随卸载停、运行中起点可改）、`sync/mod.rs:484`（每轮清 cancel 旗）、`useSyncController.ts` 补拉事件污染 notice、commands/sync.rs:141-161（reset/retry 不进 sync_command_lock）
- 为什么值得修：一轮补拉最坏数十分钟且顶栏无取消入口；离开设置页循环仍在后台 invoke；补拉进度弹到顶栏 notice 与 syncState 脱节。
- 触发条件：大跨度补拉中想停或离开页面。
- 方向：停止走后端 cancel；循环绑生命周期；补拉事件走 controller 或在面板内自洽。

### [ ] B7 认证变更与同步串行化（原 #47 #26 + P2#88 #91；#25 #27 为 P2 子弹）
- 级别：P1
- 位置：`commands/auth.rs:31-77/137-168`（save/clear 各自多把独立锁、不 `request_cancel`）、`login.rs:489-492` 同链、`:452-509`（持久化段无 epoch 复查）；`auth/mod.rs:526-563/679-703`（旧账号凭据孤儿、auth.json 缺失时 hint 不读即删）
- 为什么值得修：换账号/断开时在跑的同步拿被撤销的凭据继续写库最长 ~20 分钟且从此无法取消；A→B 换账号后 A 的 keyring 条目永久残留；幽灵会话可写进显示为 B 的库。
- 触发条件：同步进行中点断开/换账号；崩溃窗口期删 auth.json。
- 方向：save/clear 持一把覆盖全程的锁，先 `request_cancel` 再换 manager；clear 兜底读 hint 文件。
- 验收门：同步中 clear_auth，断言旧同步被取消且后续无写入。

### [ ] B8 登录窗 epoch 竞态（原 #45 改写版 #46 + P2#92）
- 级别：P2
- 位置：`login.rs:258/394`（超时/失败路径按标签关新会话窗；成功路径已复查）、`:110-143`（close-wait 后无 epoch 复查 → 幽灵窗+永卡 waiting）、`:509`（connected 前串行 await fetch_devices 最坏 ~110s）
- 为什么值得修：取消/重连的窄竞态里旧任务关掉新登录窗、或建起无人读凭据的幽灵窗且前端永卡 waiting。
- 触发条件：登录窗关闭动作挂起期间点取消/再点开始。
- 方向：关窗前与 emit 前统一复查 epoch；profile 刷新挪到 connected 之后。

### [ ] B9 controller 生命周期（P2#70 #71 #73 #74）
- 级别：P2
- 位置：`useSyncController.ts:449-462`（dispose 撞在飞 initialize → 监听器叠加）、`:300-305/331-333`（deferred/首跑 timer 逃出 dispose）、`:254-256`（unavailable/unverified 计入失败名单）、`:362`（history/initial 不查 connected）
- 为什么值得修：重复进出页面后监听器叠加、deferred 在已有成功同步后仍补一发；能力缺失被报成失败流。
- 触发条件：快速切换触发 initialize 的页面。
- 方向：initialize 加 epoch/序号守卫；所有 timer 入 dispose；失败名单只含 failed。

## C. 存储、迁移、备份恢复

### [x] C1 迁移降级无防护（原 #30 + #36）
- 2026-09-12 修复：初始化和迁移事务均拒绝更高 schema；恢复在替换当前库前复查临时快照的实际版本。回归验证 open_migrated/open_resilient/直接 migrate 拒绝且文件不变，以及兼容 manifest 掩盖新 schema 时恢复不执行。
- 级别：P1
- 位置：`migrations.rs:61-63`（无 `version > CURRENT` 分支）、`:734-744`（无条件盖 `user_version=23`）、`storage/mod.rs:1295-1310`（`version>=CURRENT` 不备份）、`backup.rs:577-680`（run_restore 不复查 user_version）vs `storage/mod.rs:1370-1388`（open_read_only 有 Greater→Err）
- 为什么值得修：v24+ 库被本构建打开会被静默降级盖章且不触发迁移前备份；被改过的 restore-pending 可让旧构建把新 schema 库当当前版读。与 open_read_only 的拒绝策略自相矛盾。
- 触发条件：装回旧版本，或新库被旧 CLI/旧构建打开。
- 方向：migrate 开头加 `version > CURRENT → Err`，与 read_only 对齐；run_restore 复查 user_version。
- 验收门：构造 user_version=CURRENT+1 的库，断言 open 报错且不盖章。

### [ ] C2 open_resilient 隔离在写锁外（原 #53 + #37）
- 级别：P1
- 位置：`storage/mod.rs:1231-1262`（quarantine 在两次 open_migrated 拿放锁之间）、`paths.rs:447-478`（relocate_entry 部分失败）、`backup.rs:660-668`（warning 被吞，隔离重建空库也报「已恢复」）
- 为什么值得修：Unix 下并发写者会把 commit 落进被挪走的 inode；Windows 下 `-wal` 搬不走时旧 WAL 帧会被重放进新库**直接损坏 B-Tree**（backup.rs:634-641 自己写明过这个场景）——比原清单说的「静默丢数据」更重。
- 触发条件：库损坏 + 另一进程正开着库 / Windows 句柄占用 / 同秒双跳迁。
- 方向：隔离手术放进写锁内；恢复路径不吞 open_resilient 的 warning。
- 验收门：恢复一个深层损坏的快照，断言报「恢复失败/库被隔离」而非成功。

### [ ] C3 恢复流程（P2#60 #67 #66 #78）
- 级别：P2
- 位置：`backup.rs:570-575`（apply_pending_restore 无锁；结尾 `let _ = cancel_pending_restore` 无条件删标记）、`:603-632`（先删 displaced 再校验）、`:660`（恢复旧 schema 快照自动再备一份 ≈3× 库大小）、`commands/backup.rs:53-59/94-98`（cancel/pin 不拿写锁）
- 为什么值得修：恢复执行被 CLI 持锁挡下时意图已被静默吞掉、下次启动不重试；残留 displaced + 校验失败的组合可丢回滚来源；磁盘 < 库大小时合法恢复整体失败。
- 触发条件：恢复期间另一进程持锁、上次恢复崩溃残留、磁盘紧张。
- 方向：恢复全程持锁（注意写锁不可重入，原 P3 有警示）；校验通过后再清理 displaced；意图只在成功后清除。

### [ ] C4 manifest id 路径穿越（P2#68）
- 级别：P2
- 位置：`backup.rs:340-362`（list_backups 不过 validate）、`:458-460`（prune 按 manifest.id 删文件）
- 为什么值得修：磁盘上被改的 manifest `{"id":"../../x"}` 可让 prune 删掉 backups/ 外的任意文件。本地攻击面，但修复便宜（list_backups 里过一次 validate_backup_id）。
- 触发条件：备份目录内 manifest 被本地进程/用户篡改。
- 方向：manifest.id 参与路径拼接前必须过 validate_backup_id。

### [ ] C5 cleanup / compact 资源（P2#39 #40）
- 级别：P2
- 位置：`storage/mod.rs:5626-5683`（十条 DELETE 无事务、checkpoint `?` 向上抛、UTC cutoff）、`:5554-5565`（全量 Vec 驻内存）
- 为什么值得修：cleanup 中途失败留半截删除；`wal_checkpoint(TRUNCATE)` 的 BUSY 把成功同步报成失败（并入 B1 的表现）；大库压缩一次读全量 payload 进内存。
- 触发条件：retention 开启且有读连接占 WAL；存量未压缩库跑手动压缩。
- 方向：DELETE 包事务；checkpoint 失败降级为警告；压缩分批流式。

### [ ] C6 数据目录搬迁（P2#95 #96 #97 + 50e）
- 级别：P2
- 位置：`paths.rs:5-13`（LEGACY_FILES 漏 restore-pending/local-api/credentials.json）、`:430+`（relocate 非原子无锁、`-wal` 失败后永久不再重试）、`app_state.rs:68`（唯一调用点——CLI/MCP 不搬迁）
- 为什么值得修：搬迁后排队恢复与本机 API 开关静默丢；搬走 .db 后 `-wal` 失败 → 下次跳过、旧 WAL 永久滞留；纯 CLI 用户升级后对着新目录的空库。
- 触发条件：从旧目录布局升级；搬迁期句柄占用。
- 方向：补搬迁清单；搬迁纳入写锁与失败重试；CLI/MCP 启动路径同样跑 relocate。

### [ ] C7 写锁一致性（原 #54 #22 P2#48 + life_events 新实例）
- 级别：P2
- 位置：`commands/data.rs` 七处写路径、`commands/sync.rs:141-161`（reset/retry）、`:249/266/285`（record_cloud_sync 直写）、**`commands/life_events.rs:15-27`（今日新增的 save/delete 同样无锁）**
- 为什么值得修：作者几乎不并发用 CLI，本组主要是一致性加固；但恢复换库是应用内路径，与持锁方交错仍可 SQLITE_BUSY 或写进将被换走的库。新功能若不复用锁约定，坑会越攒越多。
- 触发条件：GUI 写偏好/事件时 CLI 同步或恢复持锁写库。
- 方向：所有跨进程可见的写统一经 `acquire_with_timeout`（可在 core 层包一个 helper 让新命令默认走它）。

### C8 索引（P2#36 #77）—— P3
- [ ] `migrations.rs:145-152/326-338`：sleep_stages、workout_pauses 无外键索引，详情页查询与 DELETE CASCADE 随数据量线性变慢。
- [ ] `migrations.rs:186-189/660-661`：`uq_daily_metric_key`/`uq_metric_sample_key` 前缀已覆盖两个普通索引（写放大），hr_zones PK 前缀覆盖其 workout 索引；反向 `daily_metrics(metric,date)` 缺，按指标扫日期走非最优索引。

### C9 老库边角（P2#31 #37 #38）—— P3
- [ ] `migrations.rs:223-239`：v<4 分支先建 `uq_metric_sample_key` 再对 daily_metrics 去重——metric_samples 无去重步骤，v≤3 库若有撞键行则每次启动迁移失败。
- [ ] `migrations.rs:65` vs `:198-213`：version-0 非空老库走 `CREATE IF NOT EXISTS` 分支，扩列与 payload_hash 永远到不了；`backup_before_schema_change` 对 version=0 也跳过备份。
- [ ] `migrations.rs:240-245`：else 分支写死 4 列窄键 `uq_daily_metric_key`（无 COALESCE(device_id,''））——索引丢失重建时会复辟 v7 修掉的双设备覆盖 bug。这条是三件里唯一值得单独做的。
- 共同触发前提都是「假想中的早期库」，其余两件有空顺手。

## D. 界面不说谎

### [ ] D1 null→0 家族（原 #38 #41 P2#117 + P3 升级 + #62 仅 loadBand）
- 级别：P1
- 位置：`Overview.vue:464/722/723/455-456/713`、`SleepDetail.vue:198-203/232`（周堆叠 null→0、「REM: 0.0 小时」）、`TrainingStatus.vue:258/296/305` + `storage/mod.rs:4205-4234`（无数据日画 0 平线）、`WeeklyReportCard.vue:128`（`baseline_count ?? 0` 断言未知为 0）、MetricTrendCard `coverageLabel(null)` 谎称「尚未同步」、`Overview.vue:469-481/529`（loadBand 用写死 600 分档无标注）
- 为什么值得修：「缺失」被显示成 0 是健康应用最不能撒的谎——缺 REM 显示「0 分钟+假 REM 段」、没训练显示 0 负荷平线、未知基线断言「0 天有数据」。步数一侧已有「参考目标」标注（:705），负荷分档没有。
- 触发条件：云端记录缺字段、新装/无数据用户，正常使用即触发。
- 方向：null 直通 `—`/`未提供`；无数据日断线而非 0；loadBand 加「参考」标注或接真实 scale。
- 验收门：缺 REM 的睡眠记录渲染断言无 REM 段且显示「未提供」。

### [ ] D2 断线与空洞（原 #39 #40）
- 级别：P1
- 位置：`StageBar.vue:156-226`（`step:'end'` 把 slices 空洞涂成前阶段）、`HeartRateDetail.vue:168-170/188/228`（无 null 断点，`connectNulls:false` 死配置；对照 Overview.vue:388-397 已有 HR_GAP_BREAK 先例）
- 为什么值得修：摘表时段被画成直线、睡眠空洞被涂成上一阶段——图表说的是「连续正常」，事实是「没数据」。
- 触发条件：摘表/断连超过采样间隔；云端 stage 条目缺失。
- 方向：按间隔阈值插 null 断点（复用 HR_GAP_BREAK 模式）；StageBar 对空洞显式画 unknown 档。
- 验收门：两小时采样空洞的曲线断言断开而非连线。

### [ ] D3 假成功与死错误分支（原 #32 #33 #42 #58 #56 #59 P2#101 #106 #120 #107）
- P1 子弹：`HealthCheck.vue:499-531` + `useSyncController.ts:336-421`（runSync 永不 reject →「同步」动作永报成功）、`Settings.vue:975-977`（partial/cancelled 渲染成绿色 success）、`WorkoutDetail.vue:1101-1104/1137+`（series 失败吞成空 → 导出只有表头也报「已导出」）
- P2 子弹：`RecentRecords.vue:93/142-163/224-228`（error 死状态+EmptyState 死分支）、`Explore.vue:134/238-276/500-502`（previewError 无渲染点、失败谎报「还在读」）、`Settings.vue:1715-1719` + `updateService.ts:133-137`（重试撞 guard 覆盖真错误）、`updateService.ts:46-48`（`{code,message}`→`[object Object]`，err.update.* 成死文案）、`HeartRateDetail.vue:270-281`（单侧失败静默空）、`BackupPanel.vue:459`（失败同显「没有快照」）、`RecentRecords.vue:146-151`（预览态静默「暂无记录」）
- 为什么值得修：一组同构问题——失败被吞后 UI 只剩成功或空态文案，错误分支写了却不可达。
- 触发条件：对应 IPC 失败、未连接时点同步、库不可读。
- 方向：让失败可达（reject 或显式 error 态），死分支要么接上要么删。
- 验收门（P1 子弹）：mock runSync 失败，断言动作提示失败文案而非「已同步」。

### [ ] D4 状态不刷新/串扰（原 #43 + P2#102 #105 #108 #110 #111 #112 #113 #104）—— P2
- `WorkoutDetail.vue:1171-1180`（换路由不重取 insight）+ `:1121-1130`（改类型后不刷 insight）；七处 `load()` 无并发守卫（BodyStatus/HeartRateDetail:260-282/ActivityDetail/TrainingStatus/RecentRecords:142-163/HeartRateZonePicker/HealthCheck:414-424）；`Settings.vue:690-693`（诊断表单两面板绑同组 ref）；`:1116`（`:key=source.name` 撞键）；`commands/auth.rs` save_auth 不重置 login.status → 设置页残留 failed；`HealthCheck.vue:585`（`last_cloud_sync_outcome` 码直渲）、`:522`（`location.hash=''` 死代码不导航）；`DevicePicker.vue:154`（canonical key 与 catalog_id 不等 → 错选/预选失效）
- 一句话：都是「界面显示的状态落后于真实状态」或「显示不该显示的原码」。

### [ ] D5 导出与偏好承诺（原 #55 #61 —— P1；#57 —— P3）
- `useExport.ts:270-285` + `Explore.vue:547-549/567-569`：**P1**——「锁定该条运动」横幅下文件导出仍落全日期段，`focusedWorkoutId` 从不进 `exportSelection`；`Settings.vue:719-745/564-570/381-385`：**P1**——保存取消/失败不回滚，未保存的 retentionDays 草稿值直接驱动 cleanup 删除。
- `Settings.vue:274-277` + `Explore.vue:117`：**P3**——「默认导出格式」是无消费者的死开关，要么接上要么删。
- 验收门（#61）：保存失败后 cleanup 断言用已持久化的值而非草稿值。

### [ ] D6 i18n 漏洞与门禁（原 #49 #50 + P2#123 #124 #76 + 55e）
- 级别：P1
- 位置：`check-i18n.mjs:221`（PROSE_FIELDS 漏 error/refresh_error/blocker/name/display_name）、四处直渲点（Settings.vue:604/786→1644、:410-413、BackupPanel.vue:521、哨兵「融合来源/设备未确定」经 useDevices.ts:309→SleepDetail/WorkoutDetail:1347）、`data.rs:1048/1069`（剪贴板夹中文）、`HealthCheck.vue:460-464`（未知 action code 回显后端中文）、`ipc_types.rs` StreamStatusView 无 message_code、`errors.ts:74-81`（DesktopUnavailable 分支在 CJK 门后永不可达）、`insight/mod.rs:588`（`unit:"次"`）
- 为什么值得修：英文/西文界面下一批后端中文原文直接糊脸；门禁测试只扫 CoverageLedger 一种载荷，漏网是制度性的。
- 触发条件：非中文界面遇到这些字段。
- 方向：PROSE_FIELDS 补字段名或改用「白名单码 + 兜底」；ledger 测试泛化到全部 IPC 返回类型；给上述字段配 err.* 码或英文原文。
- 验收门：门禁扩展——新增任一无 `_code` 的非英文字段进 IPC 载荷即测试失败。

### [ ] D7 可访问性（原 #60 + P2#122）—— P3
- `DevicePicker.vue:173-179`：根 `tabindex=0` + 无 target 检查的 `keydown.left/right.prevent` 劫持搜索框光标。`HeartRateZonePicker.vue`：radiogroup 只有 role 壳，无 roving tabindex/方向键。

### [ ] D8 落地页（P2#114 #115）—— P2
- `useLandingLocale.ts:53/87`：localStorage 裸调，隐私模式抛异常连累 `loadLatestRelease`；DeviceMarquee：`image_key:null` 的 active 条目渲染 `img src=""` 破图。（P2#116 首帧中文属纯观感，见附录 1。）

## E. 桌面壳、更新、资源

### [ ] E1 更新链（P2#63 升 P1 + #64 #56 #59 #75 + P3×2）
- P1 子弹：`updates.rs:38-53`——`is_portable_update` 只做 exe 路径字符串比较，MSI/NSIS 安装版必判 portable → 更新在 LocalAppData 装出第二份（原 P2#63，按后果升 P1）。
- P2 子弹：`updates.rs:56-77`（拉起迁移版只看文件存在不校验版本；含 #18 主线程 sleep 15s）；`updateService.ts:102-106`（24h 窗口内没查就置 upToDate）；`:145-153`（downloadAndInstall 无超时，stall 永卡 downloading）；`:84/141/156`（三命令绕过 bridge 直 invoke，web 端无桩）；`[object Object]` 与重试死路见 D3。
- 验收门（#63）：模拟 Program Files 安装路径，断言 is_portable_update=false。

### [ ] E2 IPC 与主线程阻塞（原 #17 #18 P2#57 #58 #59 #94）—— P2
- `commands/data.rs:340-341/384-388/447-448`：compact/reprocess/integrity 三命令在 `state.db.lock().await` 内跑分钟级同步 rusqlite 操作，期间所有数据 IPC 排队。
- `updates.rs:56-77`：`launch_migrated_install` 同步命令在主线程 `thread::sleep` 轮询最多 15s。
- `lib.rs:321-323` + `open_migrated`→`backup_before_schema_change`：启动路径同步跑全库 SHA-256 校验/备份/搬迁，大库上双击图标到出窗口可隔数十秒。
- 命令层全仓无 `spawn_blocking`；keyring/fs/SQLite 都在 async fn 内同步执行，Linux SecretService 解锁可卡死 tokio worker。
- `write_lock.rs:146-171`：`acquire_with_timeout` 在 async worker 上做 `thread::sleep` 轮询。
- `build_ai_export`（storage/mod.rs:4589+）：String→Value→产物三层物化且全程占 db 锁，大导出期间所有读命令排队。
- 一句话：任何「大」操作都会冻住全部数据 IPC 或窗口；修法统一为 spawn_blocking + 锁粒度收窄。

### [ ] E3 本地 API（原 #19 #35 + 56a）—— P2
- `local_api.rs:400/409-424/447-452/190-200`（无界 accept 队列 + 关停时持锁 join 排空 → UI 冻结）、`:440-443/244/195`（serve 线程死后 running=true 且不可重启）、`:467`（token 轮换后已入队连接仍用旧 token）。作者决定：默认关，只防冻 UI 与状态错，不做抗 DoS（slowloris #34 与未文档化路由等归附录 1）。

### [ ] E4 退出与窗口—— P2 / P3
- `lib.rs:587-588` + `updates.rs:68`：**P2**——无 `ExitRequested`/`prevent_exit`，同步/备份中被托盘退出或更新直接杀进程（WAL 回滚兜底在，但不优雅）。`main_window.rs:200-222`：**P3**——WebView2 渲染进程死后只重显空白窗，无探活。

### [ ] E5 网络防御（原 #28 + 51a 51b 51g 51h）—— P2
- `zepp.rs:475`（响应体整读无上限，需服务端/TLS 内层异常才触发）、`:451-453`（重定向只比 scheme+host，漏端口/userinfo）、`:243-248`（env 代理静默生效）、`error.rs:289`（`or_else` 不取最早 URL 匹配，残段漏出）、`:245`（只有总 timeout 无 connect_timeout）。一组同源的加固项，单独都不紧急，凑一起是「客户端网络面没有纵深」。

## F. 安全红线小项

### [ ] F1 ExtractedLogin 派生 Debug 带明文 token —— P1
- 位置：`commands/login.rs:97-102`（`#[derive(Debug)]` 覆盖含 `app_token` 字段）
- 为什么值得修：任何一处 `{:?}`/日志宏就把 token 落盘，直接踩「token 不进日志」红线；修复是一行手写 Debug 脱敏。
- 触发条件：未来任何日志/错误格式化路径触达该结构体。
- 方向：手写 `impl Debug` 把 token 替换为 `***`。
- 验收门：测试断言 `format!("{:?}", extracted)` 不含 token 值。

### [ ] F2 zepp-login capability 给 core:default —— P2
- `capabilities/zepp-login.json`：`permissions:["core:default"]` 让登录窗的远端页面可 emit/listen 应用事件（伪造 `login://status` 即可触发前端 refreshStatus 与增量同步）。载荷无凭据，不是数据泄漏，是信任边界缺口——改成最小权限集。

### [ ] F3 第三方页 cookie 名进 stderr + 误判已登录 —— P2
- `login.rs:1242-1246`：`page_looks_signed_in` 对**任何页面**（含第三方 OAuth 中间页）检查 cookie 名含 token/session/userid 即判已登录，可提前收割错误会话；`:1259-1270` 的 `log_credential_probe` 把 cookie 名+host 写 eprintln——名字本身不是秘密，但属于不必要的侦查面。

### [ ] F4 CLI stderr 回落未消毒 —— P2
- `cli/main.rs:318-322`（`other.to_string()` 裸回落）+ `zepp.rs` NetworkError Display 含嵌 user_id 的 URL（原 51f 并入此条）。GUI 路径已消毒，CLI 没有。

### [ ] F5 HAR 无大小上限 / 导入不验证 —— P3
- `har.rs:24-28` 全量读入解析；`commands/auth.rs:343-365` import/manual 直接 save 不验证凭据有效性。

### [ ] F6 dead-code 埋雷四处 —— P3
- `zepp.rs:268-288`（with_client 丢防护）、`:316-323`（path_url 不拒 `//`）、`:585-588`（member_id 未校验）、`local_api.rs:153-156`（loopback 只 debug_assert）。一行决定：删掉或改成不可绕过的断言。

## G. CLI / MCP 契约（作者几乎不用；契约写在文档里所以仍要对得上）—— 全 P3，一行一条

- [ ] `cli/main.rs:292` + `storage/mod.rs:1280`：写锁 Busy 被抹成 ConfigError → 退 2 而非契约 4。
- [ ] `auth/mod.rs:907-912` + `cli:298/320-321`：无 Secret Service 退 1 + 中文原文，Headless 变体丢失。
- [ ] `cli/main.rs:289`：DataUnavailable 一刀切映 EXIT_CLOUD——「查无此 workout」也报云端失败。
- [ ] `cli/main.rs` cmd_sync：部分流失败仍退 0，契约表无 partly-failed 位置。
- [ ] `mcp/main.rs`：工具内部错误走协议级 error 而非 isError，同语义两形状。
- [ ] `mcp/main.rs` stdio：单行无上限 + 非法 UTF-8 行直接退出进程。
- （P2#86 裸值 flag 已剔除，见附录 1。）

## H. 文档与门禁漂移 —— P2/P3

- [ ] **P2**：`security-and-privacy.md:43` 声称允许 about/data/blob 中间页，实现全拒（login.rs:1165）；`development.md:210` 写「启动即绑 127.0.0.1:43921」，实际默认关+需 token；`:242` 命令契约表列 ~40 条 vs 实际注册 83 条。
- [ ] **P2**：`types.rs:226-289` 七个 Workout 字段（min_hr/elevation_*/max_cadence_spm/zepp_source）与 `WorkoutSeries.laps` Rust 恒发而 `types/index.ts` 无声明——TS 侧对这些字段的静默吞没掩盖一切消费侧问题。
- [ ] **P3**：schema 版本四文全旧——`CLAUDE.md:92` 写 21、`architecture.md:86` 与 zh-CN:44 写 16、`development.md` 写 16，实际 `CURRENT_SCHEMA_VERSION=23`（v23 落地后漂移更大）；`CLAUDE.md`「没有 tests/ 目录」已不成立（crates/cli/tests、core/tests/macos_credential_store.rs 存在）；版本处数表述统一为「不嵌数字」的写法（#128 并入）。
- [ ] **P3**：测试缺口——deviceCatalog.ts 零测试、updateService 在 vitest 覆盖外、ledger 测试只钉 CoverageLedger（并入 D6 门禁扩展）。

## I. 今日改动（2026-09-12）与审计单的交叉

### I1 已被今日提交修复（不再当待办）
| 条目 | 证据 |
|---|---|
| P2#50b HAR host 子串匹配 | 7e2227f：精确白名单 + 同 entry 绑定 token/user_id/host（har.rs:66-102） |
| P3 har.rs 跨 entry 拼接 | 同上 |
| P2#98 macOS .app 数据写进 bundle | 7e2227f：paths.rs:69-75 分流 + migrate_bundle_data |
| P2#121 / backup 补刀（部分） | 1e2ad3d：workout_samples、life_events 入 COUNTED_TABLES（backup.rs:31-38），life_events 入 TABLE_KEYS（BackupPanel.vue:290） |

### I2 生活事件（1e2ad3d）重新踩到的坑
- [ ] **新 bug**：`MetricTrendCard.vue:119-121` `Object.assign(chartSeries[chartSeries.length-1], …)` 在 hasTrend 为真但 series 为空数组的边角对 undefined 抛异常。
- [ ] `commands/life_events.rs:15-27`：save/delete 不拿跨进程写锁 → C7 新增两实例。
- [ ] `MetricTrendCard.vue:120-121`：`#D9E99B`/`#171A18` 硬编码色未走 echartsTheme 语义色。
- [ ] CSV 公式注入防护只加在 life_event 新列，`device_label` 等旧自由文本列仍裸写（export_formats.rs:58-63 对照）。
- [ ] TABLE_KEYS 半修：`daily_metrics`/`workout_samples` 仍漏、`daily_summaries` 死键还在（P2#121 维持）。
- [ ] schema 版本文档漂移随 v23 加大（并入 H）。

其它读 diff 时注意到的点（只列不展开）：
- [ ] `useLifeEvents.reload()` 直调 `tauriApi` 而非走 bridge 门面惯例；`failed` 置位后无区分错误的重试引导。
- [ ] `save_life_event` 的 UPDATE 信任调用方给的 `id`（无 user 概念可接受，但 id 枚举可探测行数）。
- [ ] `list_life_events` 无 LIMIT——用户自建表有界，低风险。
- [ ] markPoint 对每个 series 点扫全部事件，O(点×事件)，事件多时有可见开销。
- [ ] 导出把 life_events 当普通数据流计数，与 workout-scope 下 DAY_LEVEL_TYPES 的排除逻辑口径不同，已用 `calendar_context_included` 标注，边界尚可。

### I3 做得对的
- 三命令四处注册齐全（lib.rs:501-503 / types.ts:51-53 / tauri.ts:66-68 / web.ts:9-11）。
- `CHECK(end_date>=start_date)` 字符串比较安全：写入侧 `valid_date` 强制严格 YYYY-MM-DD；`end_date IS NULL`（进行中）在 list 过滤的 overlap 语义正确。
- AI 导出的 `interpretation`/`source_scope:"user_authored"` 是机器契约文本，不翻译是对的。

## 附录 1：剔除清单

| 原编号 | 剔除理由 |
|---|---|
| #44 诊断报告载荷远超文案 | 作者决定：现状可接受（用户 opt-in 一次性提交，后续收紧文案即可，不进待办） |
| #34 slowloris、56c 未文档化 `GET /`、56d RST 吃响应 | 作者决定：本机 API 默认关，不做抗 DoS |
| #29 with_client、51c path_url、51d member_id、56b debug_assert | dead code 埋雷，已并入 F6 一行 |
| 51e 结构化 status 丢失 | 已被证伪：`is_needs_reauth()`/`is_unavailable()` 谓词与独立 err.* 码都在 |
| P2#116 落地页首帧中文、P3 硬编码 hex/px 计数、死 CSS 类 | 纯观感/样式债，不进路线图（hex 只保留了 life_events 新踩的一例） |
| P2#69 无同步时点取消挂横幅 | 触发面窄且无实害 |
| P2#86 CLI 裸值 flag | CLI 边角，正常使用不触发 |
| #128 版本处数 | 表述类，并入 H 的「不嵌数字」一句 |
| P2#31/37/38 老库边角 | 触发前提是假想早期库，归 C9「有空顺手」 |
| 其余 P3 未升级子项 | 按主题归一行：storage/sync 杂项（死代码、误报空间、DailyPrefix 误匹配）≈ 大致属实但无实害；视图/组件杂项（busy 缺失、半失败误报、双渲染、z-index/设计 token 残留）≈ 真实 UX 瑕疵顺手做；decoder/normalizer/insight 杂项 ≈ 边角正确性；mcp schema 声明与 clamp 口径差 ≈ 顺手对齐 |

## 附录 2：原清单「已验证无问题」的复核

复核了 8 条正面结论，5 条仍成立、3 条措辞过满：

| 结论 | 复核 |
|---|---|
| 80 命令注册表与 BridgeBackend 全量对应 | 成立但数字过时（80→83）；life_events 三命令今日已验证四处齐全 |
| bridge 方法+listen 三处齐全 | 成立（机制随新命令同步增长） |
| validate_backup_id 覆盖全部备份命令入口 | **过满**：只盖入口入参；`list_backups` 读出的 manifest.id 不过校验，P2#68 路径穿越正是从这条缝进来 |
| 锁序无反序 | **过满**：锁序本身成立，但今日新增的 save/delete_life_event 根本不拿写锁——不是反序是缺席 |
| CLI 退出码互不相同有测试钉住 | **过满**：测试只钉「互不相同」不钉语义；P2#81-#84 四处映射错误都在 |
| normalizer 造假门测试在位 | 成立（sentinel/REM/goal=0 测试在） |
| MCP limit/days/windowDays 上界封死 | 大致成立（schema 声明与 clamp 实现口径差仍在） |
| 只读连接真只读 | 成立；但 `open_without_migration` 在锁外跑 `PRAGMA journal_mode=WAL`（写 pragma），与「只读」叙事略有出入 |

## 附录 3：核实方法

本次复核为静态只读：对照基线提交 `818c9bd` 的源码、`git show 7e2227f` 与未提交改动逐条核实，未运行任何 build/test/门禁。行号按当前源码重定位，与原清单行号存在漂移（今日合并的 life-events 改动使部分文件行号再次移动）。

复核判词口径：CONFIRMED=代码与后果描述均属实；PARTIAL=机制存在但后果/触发被夸大或只在一分支成立；REFUTED=代码里有原清单没看到的防护；FIXED_TODAY=7e2227f 或 1e2ad3d 已解决。

修复后建议跑的门禁（按 CLAUDE.md）：`cargo fmt --check`、`cargo clippy --workspace --all-targets`、`cargo test --workspace`、`cargo check`（四连）+ `npm run build` + `npm run i18n:check`；新增回归测试优先覆盖各 P1 条目的「验收门」描述——原则不变：只加能挡「假成功、丢数据、错文案」的测试。
