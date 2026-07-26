import { useState, useEffect, lazy, Suspense } from "react";
import { useQuery, useQueryClient, type QueryClient } from "@tanstack/react-query";
import { storage } from "@/lib/storage";
import { listSupplierEvents } from "@/api/key-supplier";
import { LoginPage } from "@/components/login-page";
import { Toaster } from "@/components/ui/sonner";
import { ConfirmProvider } from "@/components/ui/confirm-dialog";
import { Button } from "@/components/ui/button";
import { Activity, KeyRound, Server, LogOut, Moon, Sun, ScrollText, FolderTree, ShieldAlert, DollarSign, PackageSearch } from "lucide-react";
import { TopbarTools } from "@/components/topbar-tools";

function GithubIcon({ className }: { className?: string }) {
  return (
    <svg
      viewBox="0 0 24 24"
      fill="currentColor"
      className={className}
      aria-hidden="true"
    >
      <path d="M12 .5C5.65.5.5 5.65.5 12.02c0 5.1 3.29 9.42 7.86 10.95.58.11.79-.25.79-.55 0-.27-.01-.99-.02-1.95-3.2.7-3.87-1.54-3.87-1.54-.52-1.32-1.27-1.67-1.27-1.67-1.04-.71.08-.7.08-.7 1.15.08 1.76 1.18 1.76 1.18 1.02 1.76 2.69 1.25 3.34.95.1-.74.4-1.25.72-1.54-2.55-.29-5.24-1.28-5.24-5.69 0-1.26.45-2.29 1.18-3.09-.12-.29-.51-1.46.11-3.05 0 0 .96-.31 3.16 1.18a10.95 10.95 0 0 1 5.75 0c2.2-1.49 3.16-1.18 3.16-1.18.62 1.59.23 2.76.12 3.05.74.8 1.18 1.83 1.18 3.09 0 4.42-2.69 5.39-5.26 5.68.41.36.78 1.06.78 2.14 0 1.55-.01 2.79-.01 3.17 0 .31.21.67.8.55A11.51 11.51 0 0 0 23.5 12.02C23.5 5.65 18.35.5 12 .5Z" />
    </svg>
  );
}

const Dashboard = lazy(() =>
  import("@/components/dashboard").then((m) => ({ default: m.Dashboard })),
);
const OverviewPage = lazy(() =>
  import("@/components/overview-page").then((m) => ({
    default: m.OverviewPage,
  })),
);
const ClientKeysPage = lazy(() =>
  import("@/components/client-keys-page").then((m) => ({
    default: m.ClientKeysPage,
  })),
);
const TraceLogPage = lazy(() =>
  import("@/components/trace-log-page").then((m) => ({
    default: m.TraceLogPage,
  })),
);
const GroupsPage = lazy(() =>
  import("@/components/groups-page").then((m) => ({
    default: m.GroupsPage,
  })),
);
const ErrorSnapshotPage = lazy(() =>
  import("@/components/error-snapshot-page").then((m) => ({
    default: m.ErrorSnapshotPage,
  })),
);
const ProfitPage = lazy(() =>
  import("@/components/profit-page").then((m) => ({
    default: m.ProfitPage,
  })),
);
const KeySupplierPage = lazy(() =>
  import("@/components/key-supplier-page").then((m) => ({
    default: m.KeySupplierPage,
  })),
);

type Tab = "overview" | "credentials" | "keys" | "groups" | "traces" | "snapshots" | "profit" | "supplier";

const TABS: {
  key: Tab;
  label: string;
  mobileLabel: string;
  icon: React.ReactNode;
}[] = [
  {
    key: "overview",
    label: "概览",
    mobileLabel: "概览",
    icon: <Activity className="h-3.5 w-3.5" />,
  },
  {
    key: "credentials",
    label: "凭据管理",
    mobileLabel: "凭据",
    icon: <Server className="h-3.5 w-3.5" />,
  },
  {
    key: "keys",
    label: "客户端 Key",
    mobileLabel: "Key",
    icon: <KeyRound className="h-3.5 w-3.5" />,
  },
  {
    key: "groups",
    label: "分组管理",
    mobileLabel: "分组",
    icon: <FolderTree className="h-3.5 w-3.5" />,
  },
  {
    key: "traces",
    label: "请求日志",
    mobileLabel: "日志",
    icon: <ScrollText className="h-3.5 w-3.5" />,
  },
  {
    key: "snapshots",
    label: "错误快照",
    mobileLabel: "快照",
    icon: <ShieldAlert className="h-3.5 w-3.5" />,
  },
  {
    key: "profit",
    label: "利润报表",
    mobileLabel: "利润",
    icon: <DollarSign className="h-3.5 w-3.5" />,
  },
  {
    key: "supplier",
    label: "Key 供应",
    mobileLabel: "供应",
    icon: <PackageSearch className="h-3.5 w-3.5" />,
  },
];

/** 与 index.css 里 `--background` 的实际取值对齐。 */
const THEME_COLOR = { dark: "#121417", light: "#f6f7fa" } as const;

/**
 * 手动切换主题时同步 `<meta name="theme-color">`。
 *
 * index.html 里那两条带 `prefers-color-scheme` 的 meta 只跟随系统设置，
 * 用户在站内点太阳/月亮图标时不会生效，移动端地址栏就会和页面差一个色。
 * 这里写一条不带 media 的 meta 覆盖它们（无 media 的优先级更高）。
 */
function applyThemeColor(dark: boolean): void {
  const content = dark ? THEME_COLOR.dark : THEME_COLOR.light;
  let meta = document.querySelector<HTMLMetaElement>(
    'meta[name="theme-color"]:not([media])',
  );
  if (!meta) {
    meta = document.createElement("meta");
    meta.name = "theme-color";
    document.head.appendChild(meta);
  }
  meta.content = content;
}

function readTabFromHash(): Tab {
  const h = window.location.hash.replace(/^#\/?/, "");
  if (
    h === "credentials" ||
    h === "keys" ||
    h === "groups" ||
    h === "overview" ||
    h === "traces" ||
    h === "snapshots" ||
    h === "profit" ||
    h === "supplier"
  )
    return h;
  return "overview";
}

interface AppHeaderProps {
  darkMode: boolean;
  tab: Tab;
  onLogout: () => void;
  onToggleDarkMode: () => void;
  supplierUnread: number;
}

function App() {
  const queryClient = useQueryClient();
  const app = useAppShell(queryClient);
  const supplierEvents = useQuery({
    queryKey: ["supplier-events", "header-unread"],
    queryFn: () => listSupplierEvents({ limit: 1 }),
    enabled: app.isLoggedIn,
    refetchInterval: 5000,
  });

  if (!app.isLoggedIn) {
    return <LoggedOutApp onLogin={app.handleLogin} />;
  }

  return (
    <LoggedInApp
      darkMode={app.darkMode}
      tab={app.tab}
      onLogout={app.handleLogout}
      onToggleDarkMode={app.toggleDarkMode}
      supplierUnread={supplierEvents.data?.unreadCount ?? 0}
    />
  );
}

function useAppShell(queryClient: QueryClient) {
  const [isLoggedIn, setIsLoggedIn] = useState(false);
  const [tab, setTab] = useState<Tab>(readTabFromHash);
  const [darkMode, setDarkMode] = useState(() => {
    if (typeof window !== "undefined") {
      return document.documentElement.classList.contains("dark");
    }
    return false;
  });

  useEffect(() => {
    if (storage.getApiKey()) setIsLoggedIn(true);
  }, []);

  useEffect(() => {
    const onHash = () => setTab(readTabFromHash());
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);

  // 导航靠 <a href="#/x"> + hashchange 完成，这里不再需要 switchTab。

  const handleLogin = () => setIsLoggedIn(true);
  const handleLogout = () => {
    storage.removeApiKey();
    queryClient.clear();
    setIsLoggedIn(false);
  };
  // DOM 副作用放在 effect 里而不是 setState 的 updater 里：updater 必须是纯函数，
  // StrictMode 下会被调用两次，classList.toggle 写在里面会来回翻转。
  useEffect(() => {
    document.documentElement.classList.toggle("dark", darkMode);
    applyThemeColor(darkMode);
  }, [darkMode]);

  const toggleDarkMode = () => setDarkMode((v) => !v);

  return {
    darkMode,
    handleLogin,
    handleLogout,
    isLoggedIn,
    tab,
    toggleDarkMode,
  };
}

function LoggedOutApp({ onLogin }: { onLogin: () => void }) {
  return (
    <>
      <LoginPage onLogin={onLogin} />
      <Toaster position="bottom-center" />
    </>
  );
}

function LoggedInApp({
  darkMode,
  onLogout,
  onToggleDarkMode,
  supplierUnread,
  tab,
}: AppHeaderProps) {
  return (
    <ConfirmProvider>
      {/* 跳过导航：键盘用户按第一次 Tab 就能直达正文，不必穿过 8 个导航项。
          平时用 sr-only 藏起来，获得焦点时才浮出来。 */}
      <a
        href="#main-content"
        className="sr-only rounded-full bg-primary px-4 py-2 text-sm font-medium text-primary-foreground focus:not-sr-only focus:fixed focus:left-4 focus:top-4 focus:z-[100]"
      >
        跳到主要内容
      </a>
      <AppHeader
        darkMode={darkMode}
        tab={tab}
        onLogout={onLogout}
        onToggleDarkMode={onToggleDarkMode}
        supplierUnread={supplierUnread}
      />
      <AppMain tab={tab} onLogout={onLogout} />
      <Toaster position="bottom-center" />
    </ConfirmProvider>
  );
}

function AppHeader({
  darkMode,
  onLogout,
  onToggleDarkMode,
  supplierUnread,
  tab,
}: AppHeaderProps) {
  return (
    <header className="sticky top-0 z-50 w-full glass">
      <div className="mx-auto flex h-14 max-w-[1400px] min-w-0 items-center gap-2 px-3 sm:h-16 sm:px-4 xl:px-8 2xl:max-w-[1600px]">
        <HeaderBrand tab={tab} supplierUnread={supplierUnread} />
        <HeaderActions
          darkMode={darkMode}
          onLogout={onLogout}
          onToggleDarkMode={onToggleDarkMode}
        />
      </div>
      <MobileTabs tab={tab} supplierUnread={supplierUnread} />
    </header>
  );
}

function HeaderBrand({
  supplierUnread,
  tab,
}: {
  supplierUnread: number;
  tab: Tab;
}) {
  return (
    <div className="flex min-w-0 flex-1 items-center gap-2 xl:gap-3">
      {/* 显式 width/height：CSS 类只在样式表加载后才生效，缺内在尺寸时 logo 会先撑开
          再收缩，把整条 header 顶得跳一下（CLS）。 */}
      <img
        src="/admin/kirors.png"
        alt=""
        width={36}
        height={36}
        className="size-8 shrink-0 object-contain xl:size-9"
        draggable={false}
      />
      <span className="min-w-0 truncate text-sm font-semibold tracking-tight min-[380px]:text-base">
        Kiro Admin
      </span>
      <DesktopTabs tab={tab} supplierUnread={supplierUnread} />
    </div>
  );
}

function DesktopTabs({
  supplierUnread,
  tab,
}: {
  supplierUnread: number;
  tab: Tab;
}) {
  return (
    <nav
      aria-label="主导航"
      className="ml-4 hidden items-center gap-1 rounded-full border border-border/60 p-0.5 2xl:flex"
    >
      {TABS.map((t) => (
        <TabButton
          key={t.key}
          active={tab === t.key}
          supplierUnread={supplierUnread}
          tab={t}
        />
      ))}
    </nav>
  );
}

function HeaderActions({
  darkMode,
  onLogout,
  onToggleDarkMode,
}: {
  darkMode: boolean;
  onLogout: () => void;
  onToggleDarkMode: () => void;
}) {
  return (
    <div className="flex shrink-0 items-center gap-1">
      <div className="2xl:hidden">
        <TopbarTools compact />
      </div>
      <div className="hidden items-center gap-1 2xl:flex">
        <TopbarTools />
      </div>
      <span aria-hidden="true" className="mx-1 hidden h-5 w-px bg-border/70 2xl:inline-block" />
      <GithubButton />
      {/* 纯图标按钮必须有 aria-label：title 只在悬停时给鼠标用户看，
          读屏软件对它的支持并不一致，触屏上更是完全读不到。
          （图标本身不用管，lucide-react 在没有 a11y prop 时会自动加 aria-hidden。） */}
      <Button
        variant="ghost"
        size="icon"
        onClick={onToggleDarkMode}
        title={darkMode ? "切换到浅色主题" : "切换到深色主题"}
        aria-label={darkMode ? "切换到浅色主题" : "切换到深色主题"}
        aria-pressed={darkMode}
      >
        {darkMode ? <Sun className="h-4 w-4" /> : <Moon className="h-4 w-4" />}
      </Button>
      <Button
        variant="ghost"
        size="icon"
        onClick={onLogout}
        title="退出登录"
        aria-label="退出登录"
      >
        <LogOut className="h-4 w-4" />
      </Button>
    </div>
  );
}

function GithubButton() {
  return (
    <Button
      variant="ghost"
      size="icon"
      asChild
      title="GitHub 仓库"
      className="hidden 2xl:inline-flex"
    >
      <a
        href="https://github.com/ZyphrZero/kiro.rs"
        target="_blank"
        rel="noopener noreferrer"
        aria-label="GitHub 仓库"
      >
        <GithubIcon className="h-4 w-4" />
      </a>
    </Button>
  );
}

function MobileTabs({
  supplierUnread,
  tab,
}: {
  supplierUnread: number;
  tab: Tab;
}) {
  return (
    // overscroll-x-contain：横向滑到头时不要把手势传给浏览器，
    // 否则在 iOS/Android 上会误触发「返回上一页」。
    <nav
      aria-label="主导航"
      className="mx-auto flex max-w-[1400px] items-center gap-1 overflow-x-auto overscroll-x-contain px-3 pb-2 2xl:hidden [scrollbar-width:none] [&::-webkit-scrollbar]:hidden"
    >
      {TABS.map((t) => (
        <TabButton
          key={t.key}
          active={tab === t.key}
          mobile
          supplierUnread={supplierUnread}
          tab={t}
        />
      ))}
    </nav>
  );
}

/**
 * 导航项渲染成真正的 `<a href="#/x">` 而不是 `<button onClick>`。
 *
 * 换成锚点后，Cmd/Ctrl+点击、中键点击、右键「在新标签页打开」、悬停看目标地址
 * 这些浏览器原生行为才有效——用 button 时它们全部失效。跳转本身不需要 onClick：
 * 改 hash 会触发 `hashchange`，`useAppShell` 里的监听器负责同步 tab 状态。
 */
function TabButton({
  active,
  mobile = false,
  supplierUnread,
  tab,
}: {
  active: boolean;
  mobile?: boolean;
  supplierUnread: number;
  tab: (typeof TABS)[number];
}) {
  const className = mobile
    ? "h-8 min-w-[4.25rem] flex-1 overflow-hidden rounded-full px-2 text-[11px] min-[360px]:min-w-[4.75rem] min-[390px]:px-3 min-[390px]:text-xs md:min-w-0 md:flex-none md:px-3"
    : "h-7 rounded-full px-3 text-xs";
  const label = mobile ? tab.mobileLabel : tab.label;

  return (
    <Button
      asChild
      size="sm"
      variant={active ? "default" : "ghost"}
      className={className}
    >
      <a href={`#/${tab.key}`} aria-current={active ? "page" : undefined}>
        {tab.icon}
        <span className={mobile ? "min-w-0 truncate" : undefined}>
          {label}
        </span>
        {tab.key === "supplier" && supplierUnread > 0 && (
          <span
            className="flex h-4 min-w-4 items-center justify-center rounded-full bg-destructive px-1 text-[10px] font-semibold leading-none text-destructive-foreground tabular-nums"
            aria-label={`${supplierUnread} 条未读事件`}
          >
            {supplierUnread > 99 ? "99+" : supplierUnread}
          </span>
        )}
      </a>
    </Button>
  );
}

function AppMain({ onLogout, tab }: { onLogout: () => void; tab: Tab }) {
  return (
    // 容器宽度跟 header 对齐：header 在 2xl 放宽到 1600px 以容纳整排 tab，
    // 正文若仍停在 1400px，大屏上 logo 会比正文左边缘外凸约 100px。
    // 顺带给密集表格多出一屏宽度。
    <main
      id="main-content"
      tabIndex={-1}
      className="mx-auto max-w-[1400px] scroll-mt-24 px-4 py-8 focus:outline-none md:px-8 2xl:max-w-[1600px]"
    >
      {/* aria-live：切页时懒加载有可见延迟，读屏用户需要被告知「正在加载」而不是静默一片。 */}
      <Suspense
        fallback={
          <div className="text-sm text-muted-foreground" role="status" aria-live="polite">
            加载中…
          </div>
        }
      >
        {tab === "overview" && <OverviewPage />}
        {tab === "credentials" && <Dashboard onLogout={onLogout} embedded />}
        {tab === "keys" && <ClientKeysPage />}
        {tab === "groups" && <GroupsPage />}
        {tab === "traces" && <TraceLogPage />}
        {tab === "snapshots" && <ErrorSnapshotPage />}
        {tab === "profit" && <ProfitPage />}
        {tab === "supplier" && <KeySupplierPage />}
      </Suspense>
    </main>
  );
}

export default App;
