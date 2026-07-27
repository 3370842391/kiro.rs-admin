import { useState, useEffect, useRef } from "react";
import {
  RefreshCw,
  Server,
  Plus,
  Upload,
  FileDown,
  Trash2,
  RotateCcw,
  CheckCircle2,
  Globe,
  LogIn,
  Key,
  Building2,
  MoreHorizontal,
  Activity,
  ChevronLeft,
  ChevronRight,
  AlertTriangle,
  Eye,
  EyeOff,
  Zap,
  Tags,
  ChevronDown,
  LayoutGrid,
  List,
  Search,
  X,
} from "lucide-react";

import { useQueryClient } from "@tanstack/react-query";
import { toast } from "sonner";
import { storage } from "@/lib/storage";
import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import {
  DropdownMenu,
  DropdownMenuTrigger,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
} from "@/components/ui/dropdown-menu";
import { CredentialCard } from "@/components/credential-card";
import {
  useCredentialFilters,
  useCredentialViewPrefs,
  useDashboardDialogs,
} from "@/hooks/use-dashboard-state";
import { DashboardStatsCards } from "@/components/dashboard-stats-cards";
import { AddCredentialDialog } from "@/components/add-credential-dialog";
import { BatchImportDialog } from "@/components/batch-import-dialog";
import { BatchEditCredentialDialog } from "@/components/batch-edit-credential-dialog";
import { RpmStatusBar } from "@/components/rpm-status-bar";
import { IdcLoginDialog } from "@/components/idc-login-dialog";
import { SocialLoginDialog } from "@/components/social-login-dialog";
import {
  BatchVerifyDialog,
  type VerifyResult,
} from "@/components/batch-verify-dialog";
import { CredentialResponseTestDialog } from "@/components/credential-response-test-dialog";
import { type Tier } from "@/components/subscription-badge";
import { ProxyPoolDialog } from "@/components/proxy-pool-dialog";
import { useConfirm } from "@/components/ui/confirm-dialog";
import {
  useCredentials,
  useDeleteCredential,
  useResetFailure,
  useResetAllSuccessCount,
  useSetPriority,
} from "@/hooks/use-credentials";
import { useFailureStats } from "@/hooks/use-traces";
import { useGroupOptions } from "@/hooks/use-groups";
import { useRectSelect } from "@/hooks/use-rect-select";
import { totalInFlight } from "@/lib/rpm-operations";
import { summarizeAvailableCredits } from "@/lib/credential-summary";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Checkbox } from "@/components/ui/checkbox";
import {
  DndContext,
  PointerSensor,
  useSensor,
  useSensors,
  closestCenter,
  type DragEndEvent,
} from "@dnd-kit/core";
import {
  SortableContext,
  arrayMove,
  rectSortingStrategy,
  verticalListSortingStrategy,
} from "@dnd-kit/sortable";
import {
  getCredentialBalance,
  forceRefreshToken,
  disableQuotaExceeded,
  enableOverageForAllCapable,
  exportKamCredentials,
  type CredentialsExportResponse,
} from "@/api/credentials";
import {
  extractErrorMessage,
  parseError,
  overageFailureMessage,
} from "@/lib/utils";
import type { BalanceResponse } from "@/types/api";

interface DashboardProps {
  onLogout: () => void;
  /** 当作为 Tab 嵌入到 App 中时为 true：隐藏自带顶栏与外层布局，由父 App 提供 */
  embedded?: boolean;
}

// 订阅分级筛选的可选项（key 与 detectTier 返回值一致）
const TIER_OPTIONS: { value: Tier; label: string }[] = [
  { value: "free", label: "FREE" },
  { value: "pro", label: "PRO" },
  { value: "pro_plus", label: "PRO+" },
  { value: "power", label: "POWER" },
  { value: "unknown", label: "未知/未查询" },
];
const TIER_LABELS: Record<Tier, string> = {
  free: "FREE",
  pro: "PRO",
  pro_plus: "PRO+",
  power: "POWER",
  unknown: "未知",
};

// 每页数量可选项；另有“全部”（pageSize = 0）由下拉单独追加
const PAGE_SIZE_OPTIONS = [12, 24, 48, 96] as const;

export function Dashboard({ onLogout }: DashboardProps) {
  const confirm = useConfirm();
  const dialogs = useDashboardDialogs();
  const { openBatchImport } = dialogs;
  const [selectedIds, setSelectedIds] = useState<Set<number>>(new Set());
  const [verifyDialogOpen, setVerifyDialogOpen] = useState(false);
  const [verifying, setVerifying] = useState(false);
  const [verifyDeleting, setVerifyDeleting] = useState(false);
  const [verifyProgress, setVerifyProgress] = useState({
    current: 0,
    total: 0,
  });
  const [verifyResults, setVerifyResults] = useState<Map<number, VerifyResult>>(
    new Map(),
  );
  const [responseTestOpen, setResponseTestOpen] = useState(false);
  const [responseTestIds, setResponseTestIds] = useState<number[]>([]);
  const [balanceMap, setBalanceMap] = useState<Map<number, BalanceResponse>>(
    new Map(),
  );
  const [loadingBalanceIds, setLoadingBalanceIds] = useState<Set<number>>(
    new Set(),
  );
  const [queryingInfo, setQueryingInfo] = useState(false);
  const [queryInfoProgress, setQueryInfoProgress] = useState({
    current: 0,
    total: 0,
  });
  const [batchRefreshing, setBatchRefreshing] = useState(false);
  const [batchRefreshProgress, setBatchRefreshProgress] = useState({
    current: 0,
    total: 0,
  });
  const cancelVerifyRef = useRef(false);
  const [currentPage, setCurrentPage] = useState(1);
  // 展示偏好持久化到 localStorage，由 hook 保证 setState 与写盘成对
  const {
    pageSize,
    privacyMode,
    setPageSize,
    setPrivacyMode: changePrivacyMode,
    setViewMode: changeViewMode,
    viewMode,
  } = useCredentialViewPrefs();
  // 每页数量变化后回到第 1 页——翻页游标不归 hook 管，在这里组合
  const changePageSize = (n: number) => {
    setPageSize(n);
    setCurrentPage(1);
  };
  const queryClient = useQueryClient();
  const { data, isLoading, error, refetch } = useCredentials();
  const { mutate: deleteCredential } = useDeleteCredential();
  const { mutate: resetFailure } = useResetFailure();
  const resetAllSuccess = useResetAllSuccessCount();
  const setPriority = useSetPriority();
  const { data: failureStatsMap } = useFailureStats();
  const groupOptions = useGroupOptions();
  const selectedCredentials = (data?.credentials ?? []).filter((credential) =>
    selectedIds.has(credential.id),
  );
  const selectedCount = selectedCredentials.length;
  const inFlightRequestCount = data ? totalInFlight(data.credentials) : 0;
  const availableCreditSummary = summarizeAvailableCredits(
    data?.credentials ?? [],
    balanceMap,
  );

  // 分组 / 订阅分级 / 模糊搜索：状态与过滤逻辑一起放在 hook 里
  const filters = useCredentialFilters();
  const { groupFilter, searchQuery, tierFilter, toggleTier } = filters;
  const setGroupFilter = filters.setGroupFilter;
  const setSearchQuery = filters.setSearchQuery;

  // 分页前先过滤，确保翻页粒度对应筛选结果而不是全集
  const filteredCredentials = filters.apply(data?.credentials ?? []);

  // 切换分组 / 分级筛选 / 搜索时复位到第 1 页，避免空页
  useEffect(() => {
    setCurrentPage(1);
  }, [groupFilter, tierFilter, searchQuery]);

  // pageSize === 0 表示“全部”：单页容纳全部已筛选凭据
  const effectivePageSize =
    pageSize === 0 ? Math.max(filteredCredentials.length, 1) : pageSize;
  const totalPages = Math.max(
    1,
    Math.ceil(filteredCredentials.length / effectivePageSize),
  );
  const startIndex = (currentPage - 1) * effectivePageSize;
  const endIndex = startIndex + effectivePageSize;
  const serverPageCreds = filteredCredentials.slice(startIndex, endIndex);
  // 拖拽排序的本地乐观顺序：仅当 id 集合与当前页一致时生效，否则回落到服务端顺序，
  // 避免翻页 / 数据变更后顺序错乱。
  const [pageOrder, setPageOrder] = useState<number[] | null>(null);
  const currentCredentials = (() => {
    if (!pageOrder) return serverPageCreds;
    const serverIds = new Set(serverPageCreds.map((c) => c.id));
    const orderIds = new Set(pageOrder);
    if (
      serverIds.size !== orderIds.size ||
      ![...serverIds].every((id) => orderIds.has(id))
    ) {
      return serverPageCreds;
    }
    const byId = new Map(serverPageCreds.map((c) => [c.id, c]));
    return pageOrder.map((id) => byId.get(id)!).filter(Boolean);
  })();
  const currentPageIds = currentCredentials.map((c) => c.id);
  const currentPageAllSelected =
    currentPageIds.length > 0 &&
    currentPageIds.every((id) => selectedIds.has(id));
  const allFilteredIds = filteredCredentials.map((c) => c.id);
  const allFilteredSelected =
    allFilteredIds.length > 0 &&
    allFilteredIds.every((id) => selectedIds.has(id));

  // 翻页时清掉本地排序覆盖，回到服务端顺序
  useEffect(() => {
    setPageOrder(null);
  }, [currentPage]);

  const dragSensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 5 } }),
  );

  const handleDragEnd = (event: DragEndEvent) => {
    const { active, over } = event;
    if (!over || active.id === over.id) return;
    const ids = currentCredentials.map((c) => c.id);
    const oldIndex = ids.indexOf(Number(active.id));
    const newIndex = ids.indexOf(Number(over.id));
    if (oldIndex < 0 || newIndex < 0) return;

    const newOrder = arrayMove(ids, oldIndex, newIndex);
    setPageOrder(newOrder);

    // 按新视觉顺序赋连续递增的 priority（全局位置 = startIndex + 页内索引）。
    // 不依赖原有 priority 值域：即使原值全为默认 0 / 相同，也能保证数字更新、排序持久化；
    // 跨页也不冲突（第 1 页 0..11、第 2 页 12..23）。只对实际变化的卡片发请求。
    const prevPriority = new Map(
      currentCredentials.map((c) => [c.id, c.priority]),
    );
    const updates = newOrder
      .map((id, i) => ({ id, priority: startIndex + i }))
      .filter((u) => prevPriority.get(u.id) !== u.priority);
    if (updates.length === 0) return;

    Promise.all(
      updates.map((u) =>
        setPriority.mutateAsync({ id: u.id, priority: u.priority }),
      ),
    )
      .then(() => {
        toast.success("优先级顺序已更新");
        queryClient.invalidateQueries({ queryKey: ["credentials"] });
      })
      .catch((err) => {
        toast.error("更新优先级失败: " + (err as Error).message);
        setPageOrder(null);
      });
  };

  const gridRef = useRef<HTMLDivElement | null>(null);
  const rectSelection = useRectSelect({
    containerRef: gridRef,
    itemSelector: "[data-credential-id]",
    idAttribute: "credential-id",
    enabled: currentCredentials.length > 0,
    onSelectionChange: (hits, additive) => {
      setSelectedIds((prev) => {
        if (!additive) return new Set(hits);
        const next = new Set(prev);
        hits.forEach((id) => next.add(id));
        return next;
      });
    },
  });
  const disabledCredentialCount =
    data?.credentials.filter((c) => c.disabled).length || 0;

  // 已超额且尚未禁用的数量（用于一键超额按钮）
  const quotaExceededCount = (data?.credentials || []).filter((c) => {
    if (c.disabled) return false;
    const b = balanceMap.get(c.id) || c.balance;
    if (!b) return false;
    return b.remaining <= 0 || b.usagePercentage >= 100;
  }).length;

  // 超额统计：分别计算"已开 / 未开 / 待确定"三类，便于按钮文案与决策
  const overageStats = (() => {
    let enabled = 0;
    let disabledOff = 0;
    let unknown = 0;
    let total = 0;
    for (const c of data?.credentials || []) {
      if (c.disabled) continue;
      total += 1;
      const b = balanceMap.get(c.id) || c.balance;
      if (!b) {
        // 还没拉到余额，无法判断 — 视为待定
        unknown += 1;
        continue;
      }
      // 不可开启的订阅（FREE）不参与统计
      if (b.overageCapable === false) continue;
      if (b.overageEnabled === true) enabled += 1;
      else if (b.overageCapable === true) disabledOff += 1;
      else unknown += 1;
    }
    return { enabled, disabledOff, unknown, total };
  })();
  const overageEnableableCount = overageStats.disabledOff;
  const overageRetryableCount = overageStats.disabledOff + overageStats.unknown;

  useEffect(() => {
    setCurrentPage(1);
  }, [data?.credentials.length]);

  useEffect(() => {
    if (!data?.credentials) {
      setBalanceMap(new Map());
      setLoadingBalanceIds(new Set());
      return;
    }
    const validIds = new Set(data.credentials.map((c) => c.id));
    setBalanceMap((prev) => {
      const next = new Map<number, BalanceResponse>();
      prev.forEach((v, id) => {
        if (validIds.has(id)) next.set(id, v);
      });
      return next.size === prev.size ? prev : next;
    });
    setLoadingBalanceIds((prev) => {
      if (prev.size === 0) return prev;
      const next = new Set<number>();
      prev.forEach((id) => {
        if (validIds.has(id)) next.add(id);
      });
      return next.size === prev.size ? prev : next;
    });
  }, [data?.credentials]);

  const handleLogout = () => {
    storage.removeApiKey();
    queryClient.clear();
    onLogout();
  };

  useEffect(() => {
    if (!error) return;
    const parsed = parseError(error);
    if (parsed.type === "authentication_error") {
      toast.error("登录已失效，请重新登录");
      handleLogout();
    }
  }, [error]);

  const toggleSelect = (id: number) => {
    const next = new Set(selectedIds);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    setSelectedIds(next);
  };
  const deselectAll = () => setSelectedIds(new Set());

  /** 全选 / 取消全选当前页凭据。已选中其他页的不会被清除。 */
  const toggleSelectCurrentPage = () => {
    setSelectedIds((prev) => {
      const next = new Set(prev);
      if (currentPageAllSelected) {
        currentPageIds.forEach((id) => next.delete(id));
      } else {
        currentPageIds.forEach((id) => next.add(id));
      }
      return next;
    });
  };

  /** 全选 / 取消全选所有筛选后的凭据（跨页） */
  const toggleSelectAllFiltered = () => {
    if (allFilteredSelected) {
      // 取消：仅清除筛选范围内的，保留筛选范围外的已选项
      setSelectedIds((prev) => {
        const next = new Set(prev);
        allFilteredIds.forEach((id) => next.delete(id));
        return next;
      });
    } else {
      setSelectedIds(new Set(allFilteredIds));
    }
  };

  const handleBatchDelete = async () => {
    if (selectedCount === 0) {
      toast.error("请先选择要删除的凭据");
      return;
    }
    const ids = selectedCredentials.map((credential) => credential.id);
    if (
      !(await confirm({
        title: "批量删除凭据",
        description: `确定要删除 ${ids.length} 个凭据吗？此操作无法撤销。`,
        confirmText: "删除",
        destructive: true,
      }))
    )
      return;
    let s = 0,
      f = 0;
    for (const id of ids) {
      try {
        await new Promise<void>((resolve, reject) => {
          deleteCredential(id, {
            onSuccess: () => {
              s++;
              resolve();
            },
            onError: (err) => {
              f++;
              reject(err);
            },
          });
        });
      } catch {}
    }
    if (f === 0) toast.success(`成功删除 ${s} 个凭据`);
    else toast.warning(`删除凭据：成功 ${s} 个，失败 ${f} 个`);
    deselectAll();
  };

  const handleBatchResetFailure = async () => {
    if (selectedCount === 0) {
      toast.error("请先选择要恢复的凭据");
      return;
    }
    const failedIds = selectedCredentials
      .filter((credential) => credential.failureCount > 0)
      .map((credential) => credential.id);
    if (failedIds.length === 0) {
      toast.error("选中的凭据中没有失败的凭据");
      return;
    }
    let s = 0,
      f = 0;
    for (const id of failedIds) {
      try {
        await new Promise<void>((resolve, reject) => {
          resetFailure(id, {
            onSuccess: () => {
              s++;
              resolve();
            },
            onError: (err) => {
              f++;
              reject(err);
            },
          });
        });
      } catch {}
    }
    if (f === 0) toast.success(`成功恢复 ${s} 个凭据`);
    else toast.warning(`成功 ${s} 个，失败 ${f} 个`);
    deselectAll();
  };

  const openResponseTest = (ids: number[]) => {
    setResponseTestIds(ids);
    setResponseTestOpen(true);
  };

  const handleBatchForceRefresh = async () => {
    if (selectedCount === 0) {
      toast.error("请先选择要刷新的凭据");
      return;
    }
    const enabledIds = selectedCredentials
      .filter((credential) => !credential.disabled)
      .map((credential) => credential.id);
    if (enabledIds.length === 0) {
      toast.error("选中的凭据中没有启用的凭据");
      return;
    }
    setBatchRefreshing(true);
    setBatchRefreshProgress({ current: 0, total: enabledIds.length });
    let s = 0,
      f = 0;
    for (let i = 0; i < enabledIds.length; i++) {
      try {
        await forceRefreshToken(enabledIds[i]);
        s++;
      } catch {
        f++;
      }
      setBatchRefreshProgress({ current: i + 1, total: enabledIds.length });
    }
    setBatchRefreshing(false);
    queryClient.invalidateQueries({ queryKey: ["credentials"] });
    if (f === 0) toast.success(`成功刷新 ${s} 个凭据的 Token`);
    else toast.warning(`刷新 Token：成功 ${s} 个，失败 ${f} 个`);
    deselectAll();
  };

  const handleClearAll = async () => {
    if (!data?.credentials || data.credentials.length === 0) {
      toast.error("没有可清除的凭据");
      return;
    }
    const disabled = data.credentials.filter((c) => c.disabled);
    if (disabled.length === 0) {
      toast.error("没有可清除的已禁用凭据");
      return;
    }
    if (
      !(await confirm({
        title: "清除已禁用凭据",
        description: `确定要清除所有 ${disabled.length} 个已禁用凭据吗？此操作无法撤销。`,
        confirmText: "清除",
        destructive: true,
      }))
    )
      return;
    let s = 0,
      f = 0;
    for (const c of disabled) {
      try {
        await new Promise<void>((resolve, reject) => {
          deleteCredential(c.id, {
            onSuccess: () => {
              s++;
              resolve();
            },
            onError: (err) => {
              f++;
              reject(err);
            },
          });
        });
      } catch {}
    }
    if (f === 0) toast.success(`成功清除所有 ${s} 个已禁用凭据`);
    else toast.warning(`清除已禁用凭据：成功 ${s} 个，失败 ${f} 个`);
    deselectAll();
  };

  const handleQueryCurrentPageInfo = async () => {
    if (currentCredentials.length === 0) {
      toast.error("当前页没有可查询的凭据");
      return;
    }
    const ids = currentCredentials.filter((c) => !c.disabled).map((c) => c.id);
    if (ids.length === 0) {
      toast.error("当前页没有可查询的启用凭据");
      return;
    }
    setQueryingInfo(true);
    setQueryInfoProgress({ current: 0, total: ids.length });
    // 有界并发（worker pool，与批量验活一致），逐条更新余额与进度
    let s = 0;
    let f = 0;
    let finalized = 0;
    let next = 0;
    const CONCURRENCY = 8;
    const worker = async () => {
      while (true) {
        const i = next++;
        if (i >= ids.length) return;
        const id = ids[i];
        setLoadingBalanceIds((prev) => {
          const n = new Set(prev);
          n.add(id);
          return n;
        });
        try {
          const balance = await getCredentialBalance(id);
          s++;
          setBalanceMap((prev) => {
            const n = new Map(prev);
            n.set(id, balance);
            return n;
          });
        } catch {
          f++;
        } finally {
          setLoadingBalanceIds((prev) => {
            const n = new Set(prev);
            n.delete(id);
            return n;
          });
        }
        finalized++;
        setQueryInfoProgress({ current: finalized, total: ids.length });
      }
    };
    await Promise.all(
      Array.from({ length: Math.min(CONCURRENCY, ids.length) }, () => worker()),
    );
    setQueryingInfo(false);
    if (f === 0) toast.success(`查询完成：成功 ${s}/${ids.length}`);
    else toast.warning(`查询完成：成功 ${s} 个，失败 ${f} 个`);
  };

  const handleRefreshBalance = async (id: number) => {
    setLoadingBalanceIds((prev) => {
      const n = new Set(prev);
      n.add(id);
      return n;
    });
    try {
      const balance = await getCredentialBalance(id);
      setBalanceMap((prev) => {
        const n = new Map(prev);
        n.set(id, balance);
        return n;
      });
      toast.success("余额已刷新");
    } catch (err) {
      toast.error("刷新余额失败: " + (err as Error).message);
    } finally {
      setLoadingBalanceIds((prev) => {
        const n = new Set(prev);
        n.delete(id);
        return n;
      });
    }
  };

  const handleBatchVerify = async () => {
    if (selectedCount === 0) {
      toast.error("请先选择要验活的凭据");
      return;
    }
    setVerifying(true);
    cancelVerifyRef.current = false;
    const ids = selectedCredentials.map((credential) => credential.id);
    setVerifyProgress({ current: 0, total: ids.length });

    // id → email，便于结果列表直接看到是哪个账号
    const emailById = new Map<number, string | undefined>();
    for (const c of data?.credentials ?? []) emailById.set(c.id, c.email);

    const init = new Map<number, VerifyResult>();
    ids.forEach((id) =>
      init.set(id, { id, status: "pending", email: emailById.get(id) }),
    );
    setVerifyResults(init);
    setVerifyDialogOpen(true);

    // 有界并发（无 2s 间隔）。worker pool 领取下一个 id，逐条更新结果。
    let successCount = 0;
    let finalized = 0;
    let next = 0;
    const CONCURRENCY = 8;
    const worker = async () => {
      while (true) {
        if (cancelVerifyRef.current) return;
        const i = next++;
        if (i >= ids.length) return;
        const id = ids[i];
        setVerifyResults((prev) => {
          const n = new Map(prev);
          n.set(id, { id, status: "verifying", email: emailById.get(id) });
          return n;
        });
        try {
          const balance = await getCredentialBalance(id);
          successCount++;
          setVerifyResults((prev) => {
            const n = new Map(prev);
            n.set(id, {
              id,
              status: "success",
              usage: `${balance.currentUsage}/${balance.usageLimit}`,
              email: emailById.get(id),
            });
            return n;
          });
        } catch (err) {
          setVerifyResults((prev) => {
            const n = new Map(prev);
            n.set(id, {
              id,
              status: "failed",
              error: extractErrorMessage(err),
              email: emailById.get(id),
            });
            return n;
          });
        }
        finalized++;
        setVerifyProgress({ current: finalized, total: ids.length });
      }
    };
    await Promise.all(
      Array.from({ length: Math.min(CONCURRENCY, ids.length) }, () => worker()),
    );
    setVerifying(false);
    if (!cancelVerifyRef.current)
      toast.success(`验活完成：成功 ${successCount}/${ids.length}`);
  };

  const handleCancelVerify = () => {
    cancelVerifyRef.current = true;
    setVerifying(false);
  };

  // 在批量验活窗口删除单个失败凭据
  const handleDeleteVerifyResult = (id: number) => {
    deleteCredential(id, {
      onSuccess: () => {
        setVerifyResults((prev) => {
          const n = new Map(prev);
          n.delete(id);
          return n;
        });
        toast.success(`凭据 #${id} 已删除`);
      },
      onError: (err) => toast.error("删除失败: " + extractErrorMessage(err)),
    });
  };

  // 一键删除批量验活窗口里全部失败凭据（并发删除）
  const handleDeleteFailedVerify = () => {
    const failedIds = Array.from(verifyResults.values())
      .filter((r) => r.status === "failed")
      .map((r) => r.id);
    if (failedIds.length === 0) return;
    setVerifyDeleting(true);
    let remaining = failedIds.length;
    let ok = 0;
    failedIds.forEach((id) => {
      deleteCredential(id, {
        onSuccess: () => {
          ok++;
          setVerifyResults((prev) => {
            const n = new Map(prev);
            n.delete(id);
            return n;
          });
        },
        onError: (err) =>
          toast.error(`删除 #${id} 失败: ` + extractErrorMessage(err)),
        onSettled: () => {
          remaining--;
          if (remaining === 0) {
            setVerifyDeleting(false);
            toast.success(`已删除 ${ok}/${failedIds.length} 个失败凭据`);
          }
        },
      });
    });
  };

  // 一键超额：把所有已超额（未禁用）凭据标记为 QuotaExceeded 并禁用
  const [disablingQuota, setDisablingQuota] = useState(false);
  const handleDisableQuotaExceeded = async () => {
    if (quotaExceededCount === 0) {
      toast.info('当前没有已超额的凭据，可先点击"刷新当前页余额"');
      return;
    }
    if (
      !(await confirm({
        title: "禁用已超额凭据",
        description: `确定要把 ${quotaExceededCount} 个已超额的凭据全部禁用吗？`,
        confirmText: "禁用",
        destructive: true,
      }))
    )
      return;
    setDisablingQuota(true);
    try {
      const res = await disableQuotaExceeded();
      const ok = res.disabledIds?.length || 0;
      const skip = res.skippedIds?.length || 0;
      if (ok > 0)
        toast.success(
          `已禁用 ${ok} 个已超额凭据${skip > 0 ? `，跳过 ${skip} 个` : ""}`,
        );
      else toast.warning("未找到已超额凭据（缓存可能已失效）");
      queryClient.invalidateQueries({ queryKey: ["credentials"] });
    } catch (err) {
      toast.error("一键超额失败: " + extractErrorMessage(err));
    } finally {
      setDisablingQuota(false);
    }
  };

  // 一键开启超额：调用上游 setUserPreference 把所有"可开启且未开启"的凭据开启
  const [enablingOverage, setEnablingOverage] = useState(false);
  const handleEnableOverageAll = async () => {
    if (overageEnableableCount === 0) {
      toast.info("当前没有明确「未开启超额」的凭据");
      return;
    }
    if (
      !(await confirm({
        title: "开启超额",
        description: `确定要为 ${overageEnableableCount} 个凭据开启超额吗？开启后超出额度将按 overageRate 计费。`,
        confirmText: "开启",
      }))
    )
      return;
    setEnablingOverage(true);
    try {
      const res = await enableOverageForAllCapable();
      const ok = res.enabledIds?.length || 0;
      const fail = res.failedIds?.length || 0;
      if (ok > 0 && fail === 0) toast.success(`已为 ${ok} 个凭据开启超额`);
      else if (ok > 0 && fail > 0)
        toast.warning(
          `成功 ${ok} 个，失败 ${fail} 个：${overageFailureMessage(res.failureMessages?.[0])}`,
        );
      else if (fail > 0)
        toast.error(
          `全部失败：${overageFailureMessage(res.failureMessages?.[0])}`,
        );
      else toast.info("没有需要操作的凭据");
      queryClient.invalidateQueries({ queryKey: ["credentials"] });
    } catch (err) {
      toast.error("一键开启超额失败: " + extractErrorMessage(err));
    } finally {
      setEnablingOverage(false);
    }
  };

  // 重试拉取超额状态：仅针对状态待确定的凭据批量查余额（只读，安全）。
  // 区分于「一键开启超额」——后者会调用写接口 setUserPreference，FREE 订阅会 403。
  const [refreshingOverage, setRefreshingOverage] = useState(false);
  const [refreshingOverageProgress, setRefreshingOverageProgress] = useState({
    current: 0,
    total: 0,
  });
  const handleRefreshOverageStatus = async () => {
    const targets = (data?.credentials || [])
      .filter((c) => {
        if (c.disabled) return false;
        const b = balanceMap.get(c.id) || c.balance;
        if (!b) return true;
        return b.overageCapable === undefined || b.overageCapable === null;
      })
      .map((c) => c.id);
    if (targets.length === 0) {
      toast.info("没有状态待确定的凭据");
      return;
    }
    setRefreshingOverage(true);
    setRefreshingOverageProgress({ current: 0, total: targets.length });
    let s = 0,
      f = 0;
    for (let i = 0; i < targets.length; i++) {
      const id = targets[i];
      setLoadingBalanceIds((prev) => {
        const n = new Set(prev);
        n.add(id);
        return n;
      });
      try {
        const balance = await getCredentialBalance(id);
        s++;
        setBalanceMap((prev) => {
          const n = new Map(prev);
          n.set(id, balance);
          return n;
        });
      } catch {
        f++;
      } finally {
        setLoadingBalanceIds((prev) => {
          const n = new Set(prev);
          n.delete(id);
          return n;
        });
      }
      setRefreshingOverageProgress({ current: i + 1, total: targets.length });
    }
    setRefreshingOverage(false);
    if (f === 0) toast.success(`刷新完成：成功 ${s}/${targets.length}`);
    else toast.warning(`刷新完成：成功 ${s} 个，失败 ${f} 个`);
  };

  type ExportFormat = "generic" | "account-manager";
  const [exportingFormat, setExportingFormat] = useState<ExportFormat | null>(
    null,
  );

  const exportTimestamp = () =>
    new Date()
      .toISOString()
      .replace(/[:.]/g, "-")
      .replace("T", "_")
      .slice(0, 19);

  const downloadJson = (data: unknown, filename: string) => {
    const json = JSON.stringify(data, null, 2);
    const blob = new Blob([json], { type: "application/json" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = filename;
    document.body.appendChild(a);
    a.click();
    document.body.removeChild(a);
    URL.revokeObjectURL(url);
  };

  const toGenericExport = (data: CredentialsExportResponse) =>
    data.accounts.map((account) => {
      const c = account.credentials;
      return {
        email: account.email || undefined,
        idp: account.idp || undefined,
        status: account.status || undefined,
        machineId: account.machineId || undefined,
        profileArn: account.profileArn || undefined,
        authMethod: c.authMethod || undefined,
        provider: c.provider || account.idp || undefined,
        refreshToken: c.refreshToken,
        accessToken: c.accessToken || undefined,
        expiresAt: c.expiresAt || undefined,
        clientId: c.clientId || undefined,
        clientSecret: c.clientSecret || undefined,
        region: c.region || undefined,
        startUrl: c.startUrl || undefined,
        tokenEndpoint: c.tokenEndpoint || undefined,
        issuerUrl: c.issuerUrl || undefined,
        scopes: c.scopes || undefined,
      };
    });

  const handleExportCredentials = async (format: ExportFormat) => {
    if (selectedCount === 0) {
      toast.info("请先勾选要导出的凭据");
      return;
    }
    const ids = selectedCredentials.map((credential) => credential.id);
    setExportingFormat(format);
    try {
      const exportData = await exportKamCredentials(ids);
      const accountCount = exportData.accounts?.length ?? 0;
      if (accountCount === 0) {
        toast.warning("勾选的凭据中没有可导出的（缺少 refreshToken）");
        return;
      }
      const ts = exportTimestamp();
      if (format === "generic") {
        downloadJson(toGenericExport(exportData), `kiro-credentials-${ts}.json`);
      } else {
        downloadJson(exportData, `kiro-account-manager-export-${ts}.json`);
      }
      const skipped = ids.length - accountCount;
      const label = format === "generic" ? "通用 JSON" : "Account Manager JSON";
      toast.success(
        skipped > 0
          ? `已导出 ${accountCount} 个账号为${label}，${skipped} 个无效已跳过`
          : `已导出 ${accountCount} 个账号为${label}`,
      );
    } catch (err) {
      toast.error("导出失败: " + extractErrorMessage(err));
    } finally {
      setExportingFormat(null);
    }
  };

  if (isLoading) {
    return (
      <div className="min-h-screen flex items-center justify-center">
        <div className="text-center">
          <div className="animate-spin rounded-full h-10 w-10 border-2 border-primary/20 border-t-primary mx-auto mb-4"></div>
          <p className="text-sm text-muted-foreground">加载中…</p>
        </div>
      </div>
    );
  }

  if (error) {
    return (
      <div className="min-h-screen flex items-center justify-center p-4">
        <Card className="w-full max-w-md">
          <CardContent className="pt-6 text-center">
            <div className="text-destructive font-semibold mb-2">加载失败</div>
            <p className="text-sm text-muted-foreground mb-4">
              {extractErrorMessage(error)}
            </p>
            <div className="flex gap-2 justify-center">
              <Button onClick={() => refetch()}>重试</Button>
              <Button variant="outline" onClick={handleLogout}>
                重新登录
              </Button>
            </div>
          </CardContent>
        </Card>
      </div>
    );
  }

  return (
    <>
      {/* 这里不能再用 <main>：App 已经渲染了 <main id="main-content">，
          嵌套两个 main 地标会让读屏的「跳到主要内容」出现两个候选。 */}
      <div ref={gridRef}>
        {/* 大标题 */}
        <div className="mb-5 flex items-end justify-between gap-4 sm:mb-6">
          <div>
            <h1 className="text-2xl font-semibold tracking-tight leading-tight sm:text-[28px]">
              凭据管理
            </h1>
            <p className="mt-1 text-sm text-muted-foreground">
              管理 Kiro 的所有访问凭据、负载均衡与登录信息
            </p>
          </div>
        </div>

        <DashboardStatsCards
          available={data?.available}
          currentId={data?.currentId}
          total={data?.total}
        />

        {/* 工具栏 */}
        <div className="mb-5 flex flex-col gap-3">
          <div className="flex min-w-0 flex-wrap items-center gap-2">
            <h2 className="text-lg font-semibold tracking-tight">凭据列表</h2>
            {data?.credentials && data.credentials.length > 0 && (
              <Badge variant="secondary">
                {groupFilter || tierFilter.size > 0
                  ? `${filteredCredentials.length} / ${data.credentials.length}`
                  : data.credentials.length}
              </Badge>
            )}
            {groupFilter && (
              <Badge variant="outline" className="gap-1">
                筛选：{groupFilter === "__none__" ? "未分组" : groupFilter}
                <button
                  type="button"
                  className="ml-1 rounded-full text-muted-foreground transition-colors hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/45"
                  onClick={() => setGroupFilter("")}
                  title="清除筛选"
                  aria-label="清除分组筛选"
                >
                  <X className="h-3 w-3" />
                </button>
              </Badge>
            )}
            {tierFilter.size > 0 && (
              <Badge variant="outline" className="gap-1">
                分级：
                {Array.from(tierFilter)
                  .map((t) => TIER_LABELS[t])
                  .join("、")}
                <button
                  type="button"
                  className="ml-1 rounded-full text-muted-foreground transition-colors hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/45"
                  onClick={() => filters.clearTiers()}
                  title="清除分级筛选"
                  aria-label="清除分级筛选"
                >
                  <X className="h-3 w-3" />
                </button>
              </Badge>
            )}

            {currentCredentials.length > 0 && (
              <Button
                size="sm"
                variant="ghost"
                className="px-2 sm:px-3"
                onClick={toggleSelectCurrentPage}
                title={currentPageAllSelected ? "取消选择当前页" : "全选当前页"}
              >
                {currentPageAllSelected ? "取消全选" : "全选当前页"}
              </Button>
            )}
            {filteredCredentials.length > currentCredentials.length && (
              <Button
                size="sm"
                variant="ghost"
                className="px-2 sm:px-3"
                onClick={toggleSelectAllFiltered}
                title={
                  allFilteredSelected
                    ? "取消选择全部筛选结果"
                    : `全选所有 ${filteredCredentials.length} 个筛选结果`
                }
              >
                {allFilteredSelected
                  ? "取消全选所有页"
                  : `全选所有页 (${filteredCredentials.length})`}
              </Button>
            )}
            {selectedCount > 0 && (
              <>
                <Badge variant="default">已选 {selectedCount}</Badge>
                <Button
                  onClick={deselectAll}
                  size="sm"
                  variant="ghost"
                  className="px-2 sm:px-3"
                >
                  取消选择
                </Button>
              </>
            )}
            {verifying && !verifyDialogOpen && (
              <Button
                onClick={() => setVerifyDialogOpen(true)}
                size="sm"
                variant="secondary"
              >
                <CheckCircle2 className="h-3.5 w-3.5 animate-spin" />
                验活中… {verifyProgress.current}/{verifyProgress.total}
              </Button>
            )}
          </div>

          {/* 第二行：筛选（左） + 操作（右） */}
          <div className="flex w-full flex-col gap-2 sm:flex-row sm:flex-wrap sm:items-center">
            {/* 筛选器 — 左（移动端两列网格并排，桌面端内联） */}
            <div className="grid w-full grid-cols-2 gap-2 sm:flex sm:w-auto sm:flex-wrap sm:items-center">
              {/* 模糊搜索：来源渠道（备注）/ 邮箱；移动端整行、桌面端 200px */}
              <div className="relative col-span-2 sm:col-span-1 sm:w-[200px]">
                <Search className="pointer-events-none absolute left-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted-foreground" />
                <input
                  type="search"
                  aria-label="搜索凭据"
                  autoComplete="off"
                  spellCheck={false}
                  value={searchQuery}
                  onChange={(e) => setSearchQuery(e.target.value)}
                  placeholder="搜索来源渠道 / 备注 / 邮箱…"
                  className="h-8 w-full rounded-full border border-border bg-card/60 pl-5 pr-5 text-base backdrop-blur placeholder:text-muted-foreground/70 focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring sm:text-sm"
                />
                {searchQuery && (
                  <button
                    type="button"
                    onClick={() => setSearchQuery("")}
                    className="absolute right-2 top-1/2 flex h-5 w-5 -translate-y-1/2 items-center justify-center rounded-full text-muted-foreground transition-colors hover:bg-accent hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/45"
                    title="清除搜索"
                    aria-label="清除搜索"
                  >
                    <X className="h-3.5 w-3.5" />
                  </button>
                )}
              </div>
              <Select
                value={groupFilter || "all"}
                onValueChange={(v) => setGroupFilter(v === "all" ? "" : v)}
              >
                <SelectTrigger
                  className="h-8 w-full rounded-full border-border bg-card/60 px-3 backdrop-blur sm:w-[140px]"
                  title="按分组筛选凭据"
                  aria-label="按分组筛选凭据"
                >
                  <SelectValue placeholder="全部分组" />
                </SelectTrigger>
                <SelectContent align="end">
                  <SelectItem value="all">全部分组</SelectItem>
                  <SelectItem value="__none__">未分组</SelectItem>
                  {groupOptions.map((g) => (
                    <SelectItem key={g} value={g}>
                      {g}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>

              {/* 订阅分级筛选（多选） */}
              <DropdownMenu modal={false}>
                <DropdownMenuTrigger asChild>
                  <button
                    type="button"
                    title="按订阅分级筛选凭据（可多选，依据最近一次余额缓存）"
                    className="inline-flex h-8 w-full items-center justify-between gap-1 rounded-full border border-border bg-card/60 px-3 text-sm backdrop-blur hover:bg-accent sm:w-[136px]"
                  >
                    <span className="truncate">
                      {tierFilter.size > 0
                        ? `分级 ·${tierFilter.size}`
                        : "全部分级"}
                    </span>
                    <ChevronDown className="h-3.5 w-3.5 opacity-60" />
                  </button>
                </DropdownMenuTrigger>
                <DropdownMenuContent align="end" className="min-w-[10rem]">
                  <DropdownMenuLabel>订阅分级</DropdownMenuLabel>
                  {TIER_OPTIONS.map((t) => (
                    <DropdownMenuItem
                      key={t.value}
                      onSelect={(e) => {
                        e.preventDefault();
                        toggleTier(t.value);
                      }}
                      className="gap-2"
                    >
                      <Checkbox checked={tierFilter.has(t.value)} />
                      <span>{t.label}</span>
                    </DropdownMenuItem>
                  ))}
                  {tierFilter.size > 0 && (
                    <>
                      <DropdownMenuSeparator />
                      <DropdownMenuItem
                        onSelect={(e) => {
                          e.preventDefault();
                          filters.clearTiers();
                        }}
                        className="text-muted-foreground"
                      >
                        清除分级筛选
                      </DropdownMenuItem>
                    </>
                  )}
                </DropdownMenuContent>
              </DropdownMenu>

              {/* 卡片 / 列表 视图切换（iOS 分段控件） */}
              <div className="col-span-2 inline-flex h-8 shrink-0 items-center justify-self-start rounded-full border border-border bg-card/60 p-0.5 backdrop-blur sm:col-span-1">
                <button
                  type="button"
                  onClick={() => changeViewMode("card")}
                  aria-pressed={viewMode === "card"}
                  title="卡片视图"
                  className={`inline-flex h-7 items-center gap-1 rounded-full px-2.5 text-[13px] transition-colors ${
                    viewMode === "card"
                      ? "bg-background text-foreground shadow-apple-sm"
                      : "text-muted-foreground hover:text-foreground"
                  }`}
                >
                  <LayoutGrid className="h-3.5 w-3.5" />
                  <span className="hidden sm:inline">卡片</span>
                </button>
                <button
                  type="button"
                  onClick={() => changeViewMode("list")}
                  aria-pressed={viewMode === "list"}
                  title="列表视图"
                  className={`inline-flex h-7 items-center gap-1 rounded-full px-2.5 text-[13px] transition-colors ${
                    viewMode === "list"
                      ? "bg-background text-foreground shadow-apple-sm"
                      : "text-muted-foreground hover:text-foreground"
                  }`}
                >
                  <List className="h-3.5 w-3.5" />
                  <span className="hidden sm:inline">列表</span>
                </button>
              </div>
              {/* 显示已禁用：判死账号会在保留期内留在池子里，按线上封号速率可能积压
                  上百条。关掉只看在服务的号，与「治理设置」里的保留期形成双重控制。 */}
              <button
                type="button"
                onClick={() => filters.setShowDisabled(!filters.showDisabled)}
                aria-pressed={filters.showDisabled}
                title={
                  filters.showDisabled
                    ? "隐藏已禁用账号，只看在服务的号"
                    : "显示已禁用账号（含判死留档）"
                }
                className={`inline-flex h-8 w-full items-center justify-center gap-1 rounded-full border border-border bg-card/60 px-3 text-sm backdrop-blur transition-colors hover:bg-accent sm:w-auto ${
                  filters.showDisabled ? "text-muted-foreground" : "text-foreground"
                }`}
              >
                {filters.showDisabled ? (
                  <Eye className="h-3.5 w-3.5" />
                ) : (
                  <EyeOff className="h-3.5 w-3.5" />
                )}
                <span>含已禁用</span>
              </button>
              <button
                type="button"
                onClick={() => changePrivacyMode(!privacyMode)}
                aria-pressed={privacyMode}
                title={privacyMode ? "关闭隐私模式，显示完整邮箱" : "开启隐私模式，隐藏完整邮箱"}
                className={`inline-flex h-8 w-full items-center justify-center gap-1 rounded-full border border-border bg-card/60 px-3 text-sm backdrop-blur transition-colors hover:bg-accent sm:w-auto ${
                  privacyMode ? "text-foreground" : "text-muted-foreground"
                }`}
              >
                {privacyMode ? (
                  <EyeOff className="h-3.5 w-3.5" />
                ) : (
                  <Eye className="h-3.5 w-3.5" />
                )}
                <span>隐私模式</span>
              </button>
            </div>

            {/* 操作 — 右（移动端整宽两列网格，桌面端右对齐内联） */}
            <div className="ml-auto grid w-full grid-cols-2 gap-2 sm:flex sm:w-auto sm:flex-wrap sm:items-center">
              {selectedCount > 0 && (
                <>
                  <Button
                    onClick={() => dialogs.setBatchEditOpen(true)}
                    size="sm"
                    variant="outline"
                    title="批量编辑 RPM / 分组 / 来源渠道"
                    aria-label="批量编辑 RPM、分组和来源渠道"
                  >
                    <Tags className="h-3.5 w-3.5" />
                    批量编辑
                  </Button>
                  <Button
                    onClick={handleBatchDelete}
                    size="sm"
                    variant="destructive"
                    className="w-full sm:w-auto"
                    disabled={selectedCount === 0}
                  >
                    <Trash2 className="h-3.5 w-3.5" />
                    删除
                  </Button>
                  <span className="mx-1 hidden h-5 w-px bg-border/70 sm:inline-block" />
                </>
              )}

              {/* 主操作 */}
              <Button
                onClick={() => dialogs.setAddOpen(true)}
                size="sm"
                className="w-full sm:w-auto"
              >
                <Plus className="h-3.5 w-3.5" />
                添加凭据
              </Button>

              {/* 导入 / 登录折叠菜单 */}
              <DropdownMenu modal={false}>
                <DropdownMenuTrigger asChild>
                  <Button
                    size="sm"
                    variant="outline"
                    className="w-full sm:w-auto"
                  >
                    <Upload className="h-3.5 w-3.5" />
                    登录 / 导入 / 导出
                  </Button>
                </DropdownMenuTrigger>
                <DropdownMenuContent align="end">
                  <DropdownMenuLabel>登录</DropdownMenuLabel>
                  <DropdownMenuItem
                    onSelect={() => dialogs.setSocialLoginOpen(true)}
                  >
                    <LogIn />
                    Kiro Hosted 登录 (Google / GitHub)
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    onSelect={() => dialogs.setSocialLoginOpen(true)}
                  >
                    <Building2 />
                    企业 SSO (Entra ID / Azure AD) 登录
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    onSelect={() => dialogs.setIdcLoginOpen(true)}
                  >
                    <Key />
                    AWS SSO (IdC) 登录
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    onSelect={() => dialogs.setEnterpriseLoginOpen(true)}
                  >
                    <Building2 />
                    Enterprise (IAM Identity Center) 登录
                  </DropdownMenuItem>
                  <DropdownMenuSeparator />
                  <DropdownMenuLabel>导入</DropdownMenuLabel>
                  <DropdownMenuItem
                    onSelect={() => openBatchImport("json")}
                  >
                    <Upload />
                    批量导入凭据 / API Key / KAM
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    onSelect={() => handleExportCredentials("generic")}
                    disabled={exportingFormat !== null}
                  >
                    <FileDown />
                    {exportingFormat === "generic" ? "导出中…" : "导出通用 JSON"}
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    onSelect={() => handleExportCredentials("account-manager")}
                    disabled={exportingFormat !== null}
                  >
                    <FileDown />
                    {exportingFormat === "account-manager"
                      ? "导出中…"
                      : "导出 Account Manager JSON"}
                  </DropdownMenuItem>
                </DropdownMenuContent>
              </DropdownMenu>

              {/* 维护 / 危险操作折叠菜单 */}
              <DropdownMenu modal={false}>
                <DropdownMenuTrigger asChild>
                  <Button
                    size="sm"
                    variant="outline"
                    title="更多操作"
                    className="w-full sm:w-auto"
                  >
                    <MoreHorizontal className="h-3.5 w-3.5" />
                    <span className="sm:hidden">更多</span>
                  </Button>
                </DropdownMenuTrigger>
                <DropdownMenuContent align="end">
                  <DropdownMenuLabel>批量操作</DropdownMenuLabel>
                  <DropdownMenuItem
                    onSelect={handleBatchVerify}
                    disabled={selectedCount === 0}
                  >
                    <CheckCircle2 />
                    批量验活
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    onSelect={() =>
                      openResponseTest(
                        selectedCredentials.map((credential) => credential.id),
                      )
                    }
                    disabled={selectedCount === 0}
                  >
                    <Activity />
                    测试响应
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    onSelect={(e) => {
                      e.preventDefault();
                      handleBatchForceRefresh();
                    }}
                    disabled={selectedCount === 0 || batchRefreshing}
                  >
                    <RefreshCw
                      className={batchRefreshing ? "animate-spin" : ""}
                    />
                    {batchRefreshing
                      ? `刷新中… ${batchRefreshProgress.current}/${batchRefreshProgress.total}`
                      : "刷新 Token"}
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    onSelect={handleBatchResetFailure}
                    disabled={selectedCount === 0}
                  >
                    <RotateCcw />
                    恢复异常
                  </DropdownMenuItem>
                  <DropdownMenuSeparator />
                  <DropdownMenuLabel>维护</DropdownMenuLabel>
                  <DropdownMenuItem
                    onSelect={(e) => {
                      e.preventDefault();
                      handleQueryCurrentPageInfo();
                    }}
                    disabled={queryingInfo || !data?.credentials?.length}
                  >
                    <RefreshCw className={queryingInfo ? "animate-spin" : ""} />
                    {queryingInfo
                      ? `刷新中… ${queryInfoProgress.current}/${queryInfoProgress.total}`
                      : "刷新当前页余额"}
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    onSelect={() => dialogs.setProxyPoolOpen(true)}
                  >
                    <Globe />
                    IP 代理池管理
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    disabled={
                      resetAllSuccess.isPending || !data?.credentials?.length
                    }
                    onSelect={(e) => {
                      e.preventDefault();
                      resetAllSuccess.mutate(undefined, {
                        onSuccess: (res) => toast.success(res.message),
                        onError: (err) =>
                          toast.error("重置失败: " + (err as Error).message),
                      });
                    }}
                  >
                    <RotateCcw
                      className={
                        resetAllSuccess.isPending ? "animate-spin" : ""
                      }
                    />
                    重置成功次数
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    disabled={
                      enablingOverage ||
                      refreshingOverage ||
                      overageRetryableCount === 0
                    }
                    onSelect={(e) => {
                      e.preventDefault();
                      if (overageEnableableCount > 0) {
                        handleEnableOverageAll();
                      } else {
                        handleRefreshOverageStatus();
                      }
                    }}
                    title={
                      overageRetryableCount === 0
                        ? `全部 ${overageStats.enabled} 个 PRO/ENTERPRISE 凭据均已开启超额`
                        : `已开 ${overageStats.enabled} 个 / 未开 ${overageStats.disabledOff} 个 / 待确定 ${overageStats.unknown} 个`
                    }
                  >
                    {/* 三元的两个分支原本颜色完全一样，只有 animate-pulse 有区别。
                        顺便把 emerald-500 换成项目里 success 的既定取值
                        （见 badge.tsx 的 success variant）——500 在浅色模式下
                        对比度只有约 2.5:1，连图形元素的 3:1 门槛都不到。 */}
                    <Zap
                      className={`text-emerald-600 dark:text-emerald-400 ${
                        enablingOverage || refreshingOverage ? "animate-pulse" : ""
                      }`}
                    />
                    {refreshingOverage
                      ? `刷新中… ${refreshingOverageProgress.current}/${refreshingOverageProgress.total}`
                      : overageRetryableCount === 0
                        ? `全部已开启超额（${overageStats.enabled}）`
                        : overageEnableableCount > 0
                          ? `一键开启超额（${overageEnableableCount}）`
                          : `重试拉取超额状态（${overageStats.unknown}）`}
                  </DropdownMenuItem>
                  <DropdownMenuSeparator />
                  <DropdownMenuItem
                    destructive
                    disabled={disablingQuota || quotaExceededCount === 0}
                    onSelect={(e) => {
                      e.preventDefault();
                      handleDisableQuotaExceeded();
                    }}
                  >
                    <AlertTriangle />
                    一键超额禁用 ({quotaExceededCount})
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    destructive
                    disabled={disabledCredentialCount === 0}
                    onSelect={(e) => {
                      e.preventDefault();
                      handleClearAll();
                    }}
                  >
                    <Trash2 />
                    清除已禁用 ({disabledCredentialCount})
                  </DropdownMenuItem>
                </DropdownMenuContent>
              </DropdownMenu>
            </div>
          </div>
        </div>

        <RpmStatusBar
          summary={data ? data.rpmSummary : undefined}
          totalInFlight={inFlightRequestCount}
          availableCreditSummary={availableCreditSummary}
          creditsPerMinute={data?.creditsPerMinute}
        />

        {/* 列表 */}
        {data?.credentials.length === 0 ? (
          <Card>
            <CardContent className="py-16 text-center">
              <div className="mx-auto mb-3 flex h-12 w-12 items-center justify-center rounded-2xl bg-secondary text-muted-foreground">
                <Server className="h-5 w-5" />
              </div>
              <p className="text-sm text-muted-foreground">
                暂无凭据，点击右上角“添加凭据”开始
              </p>
            </CardContent>
          </Card>
        ) : (
          <>
            <DndContext
              sensors={dragSensors}
              collisionDetection={closestCenter}
              onDragEnd={handleDragEnd}
            >
              <SortableContext
                items={currentPageIds}
                strategy={
                  viewMode === "list"
                    ? verticalListSortingStrategy
                    : rectSortingStrategy
                }
              >
                <div
                  className={
                    viewMode === "list"
                      ? "flex select-none flex-col gap-2"
                      : "grid select-none gap-3 sm:gap-4 md:grid-cols-2 lg:grid-cols-3"
                  }
                >
                  {currentCredentials.map((credential) => (
                    <CredentialCard
                      key={credential.id}
                      credential={credential}
                      view={viewMode}
                      selected={selectedIds.has(credential.id)}
                      onToggleSelect={() => toggleSelect(credential.id)}
                      balance={
                        balanceMap.get(credential.id) ||
                        credential.balance ||
                        null
                      }
                      loadingBalance={loadingBalanceIds.has(credential.id)}
                      onRefreshBalance={() =>
                        handleRefreshBalance(credential.id)
                      }
                      failureStats={failureStatsMap?.[String(credential.id)]}
                      onTestResponse={(id) => openResponseTest([id])}
                      privacyMode={privacyMode}
                    />
                  ))}
                </div>
              </SortableContext>
            </DndContext>

            {filteredCredentials.length > 0 && (
              <div className="mt-6 flex flex-col items-center justify-center gap-3 sm:mt-8 sm:flex-row sm:gap-5">
                {/* 每页数量 */}
                <div className="flex items-center gap-2 text-sm text-muted-foreground">
                  <span className="whitespace-nowrap">每页</span>
                  <Select
                    value={String(pageSize)}
                    onValueChange={(v) => changePageSize(Number(v))}
                  >
                    <SelectTrigger
                      className="h-8 w-[92px] rounded-full border-border bg-card/60 px-3 backdrop-blur"
                      title="设置每页显示数量"
                      aria-label="设置每页显示数量"
                    >
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent align="center">
                      {PAGE_SIZE_OPTIONS.map((n) => (
                        <SelectItem key={n} value={String(n)}>
                          {n} 个
                        </SelectItem>
                      ))}
                      <SelectItem value="0">全部</SelectItem>
                    </SelectContent>
                  </Select>
                </div>

                {/* 翻页控件（仅多页时显示） */}
                {totalPages > 1 && (
                  <div className="flex flex-wrap items-center justify-center gap-2">
                    <Button
                      variant="outline"
                      size="sm"
                      onClick={() => setCurrentPage((p) => Math.max(1, p - 1))}
                      disabled={currentPage === 1}
                    >
                      <ChevronLeft className="h-3.5 w-3.5" />
                      上一页
                    </Button>
                    <div className="order-first w-full px-3 text-center text-sm tabular-nums text-muted-foreground sm:order-none sm:w-auto">
                      第{" "}
                      <span className="font-medium text-foreground">
                        {currentPage}
                      </span>{" "}
                      / {totalPages} 页
                      <span className="mx-1.5 text-muted-foreground/50">·</span>
                      共 {filteredCredentials.length} 个
                    </div>
                    <Button
                      variant="outline"
                      size="sm"
                      onClick={() =>
                        setCurrentPage((p) => Math.min(totalPages, p + 1))
                      }
                      disabled={currentPage === totalPages}
                    >
                      下一页
                      <ChevronRight className="h-3.5 w-3.5" />
                    </Button>
                  </div>
                )}
              </div>
            )}
          </>
        )}
      </div>

      {/* 弹窗们 */}
      <AddCredentialDialog
        open={dialogs.addOpen}
        onOpenChange={dialogs.setAddOpen}
        onBatchApiKeyImport={() => openBatchImport("api-key")}
      />
      <BatchImportDialog
        open={dialogs.batchImportOpen}
        onOpenChange={dialogs.setBatchImportOpen}
        initialMode={dialogs.batchImportMode}
      />
      <BatchEditCredentialDialog
        open={dialogs.batchEditOpen}
        onOpenChange={dialogs.setBatchEditOpen}
        credentials={selectedCredentials}
        groupOptions={groupOptions}
        onDone={deselectAll}
      />
      <SocialLoginDialog
        open={dialogs.socialLoginOpen}
        onOpenChange={dialogs.setSocialLoginOpen}
        onSuccess={() =>
          queryClient.invalidateQueries({ queryKey: ["credentials"] })
        }
      />
      <IdcLoginDialog
        open={dialogs.idcLoginOpen}
        onOpenChange={dialogs.setIdcLoginOpen}
        onSuccess={() =>
          queryClient.invalidateQueries({ queryKey: ["credentials"] })
        }
      />
      <IdcLoginDialog
        mode="enterprise"
        open={dialogs.enterpriseLoginOpen}
        onOpenChange={dialogs.setEnterpriseLoginOpen}
        onSuccess={() =>
          queryClient.invalidateQueries({ queryKey: ["credentials"] })
        }
      />
      <ProxyPoolDialog
        open={dialogs.proxyPoolOpen}
        onOpenChange={dialogs.setProxyPoolOpen}
      />
      {/* 镜像在线更新与「修改登录API密钥」两个弹窗已移除：
          它们的唯一入口是本文件此前那段独立模式顶栏，而该顶栏是死代码
          （Dashboard 全项目只以 embedded 形式渲染一次）。同名功能由 App 顶栏的
          TopbarTools 提供，那里有各自的 ImageUpdateDialog 与密钥表单。 */}

      {rectSelection.active && rectSelection.rect && (
        <div
          className="pointer-events-none fixed z-50 rounded-sm border border-primary/70 bg-primary/15"
          style={{
            left: rectSelection.rect.left,
            top: rectSelection.rect.top,
            width: rectSelection.rect.width,
            height: rectSelection.rect.height,
          }}
        />
      )}
      <BatchVerifyDialog
        open={verifyDialogOpen}
        onOpenChange={setVerifyDialogOpen}
        verifying={verifying}
        progress={verifyProgress}
        results={verifyResults}
        onCancel={handleCancelVerify}
        onDelete={handleDeleteVerifyResult}
        onDeleteFailed={handleDeleteFailedVerify}
        deleting={verifyDeleting}
        privacyMode={privacyMode}
      />
      <CredentialResponseTestDialog
        open={responseTestOpen}
        onOpenChange={setResponseTestOpen}
        credentials={data?.credentials ?? []}
        initialIds={responseTestIds}
        privacyMode={privacyMode}
      />
    </>
  );
}
