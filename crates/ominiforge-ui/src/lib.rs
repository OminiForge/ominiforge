//! Ominiforge UI — GPUI component library (theme, components, panels).
//!
//! See `doc/gpui-app.md`. Placeholder until Phase 3.2 adds the first
//! components.

#![allow(missing_docs)]

#[cfg(test)]
mod tests {
    //! 验证矩阵 B：gpui TestAppContext 无窗口测试链是否打通。
    //! 首个真实组件测试应替换此占位（见 doc/gpui-app.md §8）。

    use gpui::TestAppContext;

    #[gpui::test]
    fn headless_test_context_boots(cx: &mut TestAppContext) {
        // TestAppContext 不需要显示服务器即可构造；能跑到这里即证明
        // test-support feature 与 executor 在无图形会话下可用。
        cx.run_until_parked();
    }
}
