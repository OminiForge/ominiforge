<!-- status: current -->
<!-- owner: @duskgrow -->

# 供应链安全维护（audit / deny 豁免的生命周期）

CI 的 `just audit`（cargo audit）与 `just deny`（cargo deny）会因**传递依赖的已知漏洞**
而失败。本文规定豁免的**加入、复查、解除**全流程——重点是「解除」如何被触发，避免豁免只加不减、
烂在列表里。

> **现状（零 UI 转向后）**：gpui 及其依赖闭包已移除，此前的 gpui 传递依赖豁免全部失效，当前两个
> 豁免列表均为空。下述机制保留，供未来新增传递依赖漏洞时使用。

## 两套豁免机制（别搞混）

| 工具 | 配置文件 | CI 命令 |
| ---- | -------- | ------- |
| cargo audit | `.cargo/audit.toml` 的 `[advisories].ignore` | `just audit` |
| cargo deny  | `deny.toml` 的 `[advisories].ignore` | `just deny` |

两者独立。`cargo audit` **不读** `deny.toml`。新增豁免时按失败的是哪个命令写到对应文件。

## 加入豁免（触发：CI 红）

当 `just audit` / `just deny` 失败时：

1. **查来源**：`cargo tree -i <crate> --target all` 确认该漏洞来自哪条依赖链。
2. **判断能否豁免**：
   - 仅出现在不进入发布产物的链（如仅 dev/test 依赖）、运行时不含 → **可豁免**。
   - 影响 CLI 运行时，或我们直接依赖的 crate → **不可豁免**，必须升级或修复。
3. **写入豁免**，注释格式：`# <crate> ← <父链>（加入日期，待复查）`。
4. push 使 CI 转绿。

## 解除豁免（触发：钉在依赖 bump 上，事件驱动）

豁免的来源是某条依赖链，**来源不变复查无意义，来源变了必须复查**。因此「解除检查」钉死在
**bump 该依赖这个动作**里，而非靠日历提醒（易忘）。

**每次 bump 引入该豁免的依赖版本时，在 PR 中强制执行：**

1. 临时清空 `.cargo/audit.toml` 的 `ignore` 列表（备份原内容）。
2. 跑 `cargo audit`，观察输出：
   - **仍报**的漏洞 → 上游未修复，把对应豁免**加回**。
   - **不再报**的 → 上游已随这次 bump 修复，**保持删除**，豁免解除。
3. 恢复/更新豁免文件，连同 bump 一起提交。

复查命令（手动快速验证某条豁免是否仍需要）：

```sh
cargo audit --ignore RUSTSEC-XXXX-YYYY   # 忽略除某条外的全部太麻烦；直接清空列表重跑更直观
```

> 注：`cargo audit` 没有「忽略 ignore 列表」的开关，所以复查采用「清空列表重跑」。

## 兜底：定期复查（可选，二级保障）

若相关依赖长期未 bump，豁免可能滞留。可加一个每周定时 CI：清空豁免列表重跑 `cargo audit`，
若发现「某豁免已无必要」（不报错了），开 issue 提醒移除。**当前先不做**，待机制 1 证明
不够再补。

## 当前豁免清单

见 `.cargo/audit.toml`（cargo audit）与 `deny.toml`（cargo deny）内的逐条注释，含来源链。
本文不复制清单内容（单一事实源在配置文件里，见 CLAUDE.md 规则 12）。
