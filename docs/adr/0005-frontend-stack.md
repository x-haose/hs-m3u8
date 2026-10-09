# ADR-0005：桌面应用前端技术栈

- 状态：已采纳（具体版本在 GUI 开工时复核）
- 日期：2026-10-09

## 决定

React + TypeScript，组件体系用 shadcn/ui（底层基础组件用 Base UI），样式用 Tailwind CSS 4；动画用 Motion，状态用 Zustand，长列表用 TanStack Virtual；构建用 Vite。

## 考虑过的方案（2026-10-09 查 npm）

| 框架 | 组件体系 | 判断 |
|---|---|---|
| React 19.3.0 | shadcn（官方，CLI 4.21.4） | 采用：生态最大；作者的 x-iztro 与 hs-net 文档站均基于 React |
| Vue 3.5.43 | shadcn-vue 2.8.2（社区移植） | 可行，移植跟进官方有滞后 |
| Svelte 5.57.2 | shadcn-svelte 1.7.0 | 响应式粒度细，生态小一档 |
| Solid 1.9.17 | 少 | 生态最小 |

## 理由

- 这个界面的难点在交互细节：焦点管理、键盘操作、菜单、对话框、组合框，要做到原生应用的手感。这些由成熟的基础组件库解决，生态大小是第一考量。
- 多任务进度高频刷新不构成换框架的理由：按行订阅状态、每个任务限频推送、虚拟列表即可。
- shadcn 的组件代码复制进项目、样式完全可控，适合实现自有设计（倾向「明亮下载器」主题）。
- shadcn 官方更新日志：2026 年 7 月起默认底层组件库由 Radix 改为 Base UI。设计对比原型用的是 Radix（radix-ui 1.7.0），新项目跟随官方默认。

## 依据的版本

react 19.3.0、shadcn 4.21.4、@base-ui/react 1.9.0、radix-ui 1.7.0、tailwindcss 4.3.3、motion 14.0.0、zustand 5.0.15、@tanstack/react-virtual 3.14.13、vue 3.5.43、shadcn-vue 2.8.2、svelte 5.57.2、shadcn-svelte 1.7.0、solid-js 1.9.17。
