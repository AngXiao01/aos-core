# aos-core

AOS（agent-native OS）的运行时版：作为常驻程序直接运行在现有的 macOS 或 Linux 上，不需要虚拟机，本地作业可以使用宿主机的 GPU。

本仓库自成一体，不依赖任何其他仓库。唯一的规格文件是 [`SPEC.md`](SPEC.md)，第一部分规定系统语义，第二部分规定平台实现。

## 现状

核心库已在 `main`：§18 列出的 10 个 crate 放在 `crates/`，做到 M0–M3；§16.1 的一致性测试在内存后端上通过。未写死的选择、规格矛盾，以及存储与出口 trait 的交接，分别见 [`DECISIONS.md`](DECISIONS.md)、[`QUESTIONS.md`](QUESTIONS.md)、[`docs/trait-handoff.md`](docs/trait-handoff.md)。

## 给实现 agent

1. 先读 `SPEC.md` 的 §0，再通读全文。
2. 按 §18 的仓库布局组织代码。核心库、人的界面、iPhone 确认器都在本仓库内实现。
3. §C1 的威胁模型决定了这个版本防什么、不防什么，实现时不要越过它，也不要假装防住了范围之外的东西。
4. 决定写进 `DECISIONS.md`，矛盾写进 `QUESTIONS.md`。
