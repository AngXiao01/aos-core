# aos-core

AOS（agent-native OS）的共享核心规格，以及两个实现版本共用的核心库。

- 规格：[`00-core-spec.md`](00-core-spec.md)
- 版本 A（Linux 上的独立世界）：[AngXiao01/aos-linux](https://github.com/AngXiao01/aos-linux)
- 版本 B（Rust 能力式微内核）：[AngXiao01/aos-microkernel](https://github.com/AngXiao01/aos-microkernel)

本仓库将承载核心规格 §18 里列出的 `no_std` crate（`aos-types`、`aos-log`、`aos-branch` 等）和一致性测试套件 `aos-conformance`。两个版本都依赖这里的代码，并且必须通过同一套一致性测试。

## 给实现 agent

先完整阅读 `00-core-spec.md` 的 §0。决定写进 `DECISIONS.md`，矛盾写进 `QUESTIONS.md`。
