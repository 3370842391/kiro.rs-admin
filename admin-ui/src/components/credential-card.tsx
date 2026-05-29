import { useState, useEffect, useCallback } from "react";
import { toast } from "sonner";
import {
  RefreshCw,
  ChevronUp,
  ChevronDown,
  Trash2,
  Loader2,
  Pencil,
  LogIn,
  MoreHorizontal,
  RotateCcw,
  Zap,
  ZapOff,
  Clock,
} from "lucide-react";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Switch } from "@/components/ui/switch";
import { Input } from "@/components/ui/input";
import { Checkbox } from "@/components/ui/checkbox";
import { Progress } from "@/components/ui/progress";
import {
  DropdownMenu,
  DropdownMenuTrigger,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
} from "@/components/ui/dropdown-menu";
import { SubscriptionBadge } from "@/components/subscription-badge";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import type { CredentialStatusItem, BalanceResponse } from "@/types/api";
import { maskProxyUrl, extractErrorMessage } from "@/lib/utils";
import {
  useSetDisabled,
  useSetPriority,
  useResetFailure,
  useDeleteCredential,
  useForceRefreshToken,
  useResetSuccessCount,
  useClearThrottle,
} from "@/hooks/use-credentials";
import { setCredentialOverage } from "@/api/credentials";
import { useQueryClient } from "@tanstack/react-query";
import { EditCredentialDialog } from "@/components/edit-credential-dialog";
import { UpdateTokenDialog } from "@/components/update-token-dialog";
import { ReloginDialog } from "@/components/relogin-dialog";

interface CredentialCardProps {
  credential: CredentialStatusItem;
  selected: boolean;
  onToggleSelect: () => void;
  balance: BalanceResponse | null;
  loadingBalance: boolean;
  onRefreshBalance: () => void;
}

function formatLastUsed(lastUsedAt: string | null): string {
  if (!lastUsedAt) return "从未使用";
  const date = new Date(lastUsedAt);
  const diff = Date.now() - date.getTime();
  if (diff < 0) return "刚刚";
  const s = Math.floor(diff / 1000);
  if (s < 60) return `${s} 秒前`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m} 分钟前`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h} 小时前`;
  return `${Math.floor(h / 24)} 天前`;
}

function formatNumber(n: number): string {
  return n.toLocaleString("zh-CN", {
    minimumFractionDigits: 2,
    maximumFractionDigits: 2,
  });
}

function formatResetDate(ts: number | null): string {
  if (!ts) return "未知";
  return new Date(ts * 1000).toLocaleString("zh-CN");
}

/** 把秒数格式化为 `mm:ss` 或 `hh:mm:ss` */
function formatThrottleCountdown(secs: number): string {
  const total = Math.max(0, Math.floor(secs));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  const pad = (n: number) => String(n).padStart(2, "0");
  return h > 0 ? `${h}:${pad(m)}:${pad(s)}` : `${pad(m)}:${pad(s)}`;
}

/**
 * 紧凑超额状态胶囊 — 与订阅徽章并列展示，不占整行
 * 三态：已开（绿色实色）/ 未开（中性细描边）/ 不支持（灰色虚边小字）
 */
function OverageStatusPill({ balance }: { balance: BalanceResponse }) {
  const cap = balance.overageCapable;
  const on = balance.overageEnabled === true;

  // 不支持的订阅：极弱化
  if (cap === false) return null;

  if (on) {
    return (
      <span
        className="inline-flex items-center gap-1 rounded-full bg-emerald-500/15 px-2 h-6 text-[11px] font-medium text-emerald-700 dark:text-emerald-400"
        title="此账号已开启超额"
      >
        <Zap className="h-3 w-3" />
        超额
      </span>
    );
  }

  if (cap === true) {
    return (
      <span
        className="inline-flex items-center gap-1 rounded-full border border-amber-500/40 bg-transparent px-2 h-6 text-[11px] font-medium text-amber-600 dark:text-amber-400"
        title="此账号支持超额但当前未开启"
      >
        <ZapOff className="h-3 w-3" />
        未开
      </span>
    );
  }

  // 未知：低调灰色，hover 看原始值
  return (
    <span
      className="inline-flex items-center gap-1 rounded-full border border-dashed border-border/60 bg-transparent px-2 h-6 text-[11px] text-muted-foreground"
      title={
        balance.overageCapabilityRaw
          ? `overageCapability = ${balance.overageCapabilityRaw}`
          : "上游未返回 overageCapability"
      }
    >
      <ZapOff className="h-3 w-3" />
      未知
    </span>
  );
}

/**
 * 把后端返回的 disabledReason 字符串映射为更直观的中文徽标
 * （颜色/文案/排序权重，越靠前越显眼）
 */
function getDisabledReasonStyle(reason?: string | null): {
  label: string;
  variant: "destructive" | "warning" | "outline" | "secondary";
} | null {
  if (!reason) return null;
  switch (reason) {
    case "QuotaExceeded":
      return { label: "已超额", variant: "warning" };
    case "TooManyFailures":
      return { label: "失败过多", variant: "destructive" };
    case "TooManyRefreshFailures":
      return { label: "刷新失败过多", variant: "destructive" };
    case "InvalidRefreshToken":
      return { label: "Token 失效", variant: "destructive" };
    case "InvalidConfig":
      return { label: "配置无效", variant: "destructive" };
    case "Manual":
      return { label: "手动禁用", variant: "secondary" };
    default:
      return { label: reason, variant: "outline" };
  }
}

export function CredentialCard({
  credential,
  selected,
  onToggleSelect,
  balance,
  loadingBalance,
  onRefreshBalance,
}: CredentialCardProps) {
  const [editingPriority, setEditingPriority] = useState(false);
  const [priorityValue, setPriorityValue] = useState(
    String(credential.priority),
  );
  const [showDeleteDialog, setShowDeleteDialog] = useState(false);
  const [showEditDialog, setShowEditDialog] = useState(false);
  const [showUpdateTokenDialog, setShowUpdateTokenDialog] = useState(false);
  const [showReloginDialog, setShowReloginDialog] = useState(false);

  const setDisabled = useSetDisabled();
  const setPriority = useSetPriority();
  const resetFailure = useResetFailure();
  const deleteCredential = useDeleteCredential();
  const forceRefresh = useForceRefreshToken();
  const resetSuccess = useResetSuccessCount();
  const clearThrottle = useClearThrottle();
  const queryClient = useQueryClient();

  // 后端冷却剩余秒数会在 30s 拉取间隔之间过时，本地用 setInterval 自然递减以让倒计时连续。
  const [throttleRemaining, setThrottleRemaining] = useState<number>(
    credential.throttledRemainingSecs ?? 0,
  );
  useEffect(() => {
    setThrottleRemaining(credential.throttledRemainingSecs ?? 0);
  }, [credential.throttledRemainingSecs]);
  useEffect(() => {
    if (throttleRemaining <= 0) return;
    const t = window.setInterval(() => {
      setThrottleRemaining((v) => (v > 0 ? v - 1 : 0));
    }, 1000);
    return () => window.clearInterval(t);
  }, [throttleRemaining]);
  const handleClearThrottle = useCallback(() => {
    clearThrottle.mutate(credential.id, {
      onSuccess: (res) => {
        setThrottleRemaining(0);
        toast.success(res.message);
      },
      onError: (err) => toast.error("解除失败: " + extractErrorMessage(err)),
    });
  }, [clearThrottle, credential.id]);
  const [overageBusy, setOverageBusy] = useState(false);
  const handleSetOverage = async (enabled: boolean) => {
    setOverageBusy(true);
    try {
      await setCredentialOverage(credential.id, enabled);
      toast.success(enabled ? "已开启超额" : "已关闭超额");
      queryClient.invalidateQueries({ queryKey: ["credentials"] });
    } catch (err) {
      toast.error(
        (enabled ? "开启" : "关闭") + "超额失败: " + extractErrorMessage(err),
      );
    } finally {
      setOverageBusy(false);
    }
  };

  const handleToggleDisabled = () => {
    setDisabled.mutate(
      { id: credential.id, disabled: !credential.disabled },
      {
        onSuccess: (res) => toast.success(res.message),
        onError: (err) => toast.error("操作失败: " + (err as Error).message),
      },
    );
  };

  const handlePriorityChange = () => {
    const np = parseInt(priorityValue, 10);
    if (isNaN(np) || np < 0) {
      toast.error("优先级必须是非负整数");
      return;
    }
    setPriority.mutate(
      { id: credential.id, priority: np },
      {
        onSuccess: (res) => {
          toast.success(res.message);
          setEditingPriority(false);
        },
        onError: (err) => toast.error("操作失败: " + (err as Error).message),
      },
    );
  };

  const handleReset = () =>
    resetFailure.mutate(credential.id, {
      onSuccess: (res) => toast.success(res.message),
      onError: (err) => toast.error("操作失败: " + (err as Error).message),
    });

  const handleForceRefresh = () =>
    forceRefresh.mutate(credential.id, {
      onSuccess: (res) => toast.success(res.message),
      onError: (err) => toast.error("刷新失败: " + extractErrorMessage(err)),
    });

  const handleResetSuccess = () =>
    resetSuccess.mutate(credential.id, {
      onSuccess: (res) => toast.success(res.message),
      onError: (err) => toast.error("重置失败: " + (err as Error).message),
    });

  const handleDelete = () => {
    if (!credential.disabled) {
      toast.error("请先禁用凭据再删除");
      setShowDeleteDialog(false);
      return;
    }
    deleteCredential.mutate(credential.id, {
      onSuccess: (res) => {
        toast.success(res.message);
        setShowDeleteDialog(false);
      },
      onError: (err) => toast.error("删除失败: " + (err as Error).message),
    });
  };

  const authLabel =
    credential.authMethod === "api_key"
      ? "API Key"
      : credential.authMethod === "idc"
        ? "IdC"
        : credential.authMethod === "social"
          ? "Social"
          : credential.authMethod;

  const isQuotaExceeded = balance
    ? balance.remaining <= 0 || balance.usagePercentage >= 100
    : false;

  const disabledByQuota =
    credential.disabled && credential.disabledReason === "QuotaExceeded";
  const reasonStyle = getDisabledReasonStyle(credential.disabledReason);
  const isThrottled = !credential.disabled && throttleRemaining > 0;

  return (
    <>
      <Card
        data-credential-id={credential.id}
        className={`group hover:-translate-y-0.5 hover:shadow-apple-lg ${
          credential.isCurrent ? "ring-2 ring-primary/60 shadow-apple-lg" : ""
        } ${
          // 未禁用但已超额：琥珀色提醒边
          !credential.disabled && isQuotaExceeded
            ? "ring-1 ring-amber-500/60"
            : ""
        } ${
          // 已因超额被禁用：琥珀色实色边 + 不灰化（保留可读性，方便审视）
          disabledByQuota
            ? "ring-1 ring-amber-500/70 bg-amber-50/40 dark:bg-amber-500/[0.04]"
            : ""
        } ${
          // 账号级风控冷却中：橙红色提示边
          isThrottled
            ? "ring-1 ring-orange-500/60 bg-orange-50/40 dark:bg-orange-500/[0.04]"
            : ""
        } ${
          // 其他原因被禁用：常规灰化
          credential.disabled && !disabledByQuota ? "opacity-70" : ""
        }`}
      >
        <CardHeader className="pb-3">
          <div className="flex items-start gap-3">
            <label
              data-no-rect-select
              className="mt-0.5 flex h-7 w-7 shrink-0 cursor-pointer items-center justify-center rounded-md transition-colors hover:bg-accent"
              onClick={(e) => {
                // label + Checkbox 双击事件去重，避免触发两次 onCheckedChange
                e.stopPropagation();
              }}
            >
              <Checkbox
                className="h-5 w-5 [&_svg]:h-4 [&_svg]:w-4"
                checked={selected}
                onCheckedChange={onToggleSelect}
              />
            </label>
            <div className="flex-1 min-w-0">
              <CardTitle className="truncate text-[15px]">
                {credential.email || `凭据 #${credential.id}`}
              </CardTitle>
              <div className="mt-1.5 flex flex-wrap items-center gap-1">
                {balance?.subscriptionTitle && (
                  <SubscriptionBadge title={balance.subscriptionTitle} />
                )}
                {credential.isCurrent && <Badge variant="success">活跃</Badge>}
                {/* 禁用状态：合并 "已禁用" + 中文化的原因，单个 Badge 更醒目 */}
                {credential.disabled && reasonStyle && (
                  <Badge variant={reasonStyle.variant}>
                    已禁用 · {reasonStyle.label}
                  </Badge>
                )}
                {credential.disabled && !reasonStyle && (
                  <Badge variant="destructive">已禁用</Badge>
                )}
                {/* 仍启用但已经达到上限：黄色"已超额"徽章 */}
                {!credential.disabled && isQuotaExceeded && (
                  <Badge variant="warning">已超额</Badge>
                )}
                {isThrottled && (
                  <Badge
                    variant="warning"
                    className="bg-orange-500/15 text-orange-700 dark:text-orange-300 border-orange-500/30"
                    title="账号级风控冷却中（429 + suspicious activity），到期或手动解除后恢复调度"
                  >
                    <Clock className="mr-1 h-3 w-3" />
                    冷却 {formatThrottleCountdown(throttleRemaining)}
                  </Badge>
                )}
                {credential.authMethod && (
                  <Badge variant="secondary">{authLabel}</Badge>
                )}
                {credential.endpoint && (
                  <Badge variant="outline">{credential.endpoint}</Badge>
                )}
                {credential.hasProfileArn && (
                  <Badge variant="outline">Profile ARN</Badge>
                )}
              </div>
            </div>
            <Switch
              checked={!credential.disabled}
              onCheckedChange={handleToggleDisabled}
              disabled={setDisabled.isPending}
              title={credential.disabled ? "启用" : "禁用"}
            />
          </div>
        </CardHeader>

        <CardContent className="space-y-4">
          {/* 信息行 */}
          <dl className="grid grid-cols-2 gap-x-4 gap-y-2 text-[13px]">
            <div className="flex items-center justify-between gap-2">
              <dt className="text-muted-foreground">优先级</dt>
              <dd>
                {editingPriority ? (
                  <div className="inline-flex items-center gap-1">
                    <Input
                      type="number"
                      value={priorityValue}
                      onChange={(e) => setPriorityValue(e.target.value)}
                      className="w-16 h-7 text-sm rounded-md"
                      min="0"
                    />
                    <Button
                      size="icon"
                      variant="ghost"
                      className="h-7 w-7"
                      onClick={handlePriorityChange}
                      disabled={setPriority.isPending}
                    >
                      ✓
                    </Button>
                    <Button
                      size="icon"
                      variant="ghost"
                      className="h-7 w-7"
                      onClick={() => {
                        setEditingPriority(false);
                        setPriorityValue(String(credential.priority));
                      }}
                    >
                      ✕
                    </Button>
                  </div>
                ) : (
                  <button
                    type="button"
                    className="font-medium tabular-nums hover:text-primary transition-colors"
                    onClick={() => setEditingPriority(true)}
                    title="点击编辑"
                  >
                    {credential.priority}
                  </button>
                )}
              </dd>
            </div>
            <div className="flex items-center justify-between gap-2">
              <dt className="text-muted-foreground">失败次数</dt>
              <dd
                className={`tabular-nums font-medium ${credential.failureCount > 0 ? "text-destructive" : ""}`}
              >
                {credential.failureCount}
              </dd>
            </div>
            <div className="flex items-center justify-between gap-2">
              <dt className="text-muted-foreground">刷新失败</dt>
              <dd
                className={`tabular-nums font-medium ${credential.refreshFailureCount > 0 ? "text-destructive" : ""}`}
              >
                {credential.refreshFailureCount}
              </dd>
            </div>
            <div className="flex items-center justify-between gap-2">
              <dt className="text-muted-foreground">成功次数</dt>
              <dd>
                <button
                  type="button"
                  onClick={handleResetSuccess}
                  className="tabular-nums font-medium hover:text-primary transition-colors"
                  title="点击重置"
                >
                  {credential.successCount}
                </button>
              </dd>
            </div>
            <div className="col-span-2 flex items-center justify-between gap-2 border-t border-border/50 pt-2">
              <dt className="text-muted-foreground">最后调用</dt>
              <dd className="font-medium">
                {formatLastUsed(credential.lastUsedAt)}
              </dd>
            </div>
            {credential.maskedApiKey && (
              <div className="col-span-2 flex items-center justify-between gap-2">
                <dt className="text-muted-foreground">API Key</dt>
                <dd className="font-mono text-xs truncate">
                  {credential.maskedApiKey}
                </dd>
              </div>
            )}
            {credential.hasProxy && (
              <div className="col-span-2 flex items-center justify-between gap-2">
                <dt className="text-muted-foreground">代理</dt>
                <dd className="font-mono text-xs truncate">
                  {maskProxyUrl(credential.proxyUrl ?? "")}
                </dd>
              </div>
            )}
          </dl>

          {/* 余额面板 */}
          <div
            className={`rounded-xl border p-4 transition-colors ${
              isQuotaExceeded || disabledByQuota
                ? "border-amber-500/40 bg-amber-50/60 dark:bg-amber-500/[0.06]"
                : "border-border/60 bg-secondary/40"
            }`}
          >
            {loadingBalance ? (
              <div className="flex items-center justify-center gap-2 py-3 text-sm text-muted-foreground">
                <Loader2 className="h-4 w-4 animate-spin" />
                正在查询余额…
              </div>
            ) : balance ? (
              <div className="space-y-3">
                <div className="flex items-end justify-between gap-3">
                  <div className="min-w-0">
                    <div className="text-[11px] uppercase tracking-wider text-muted-foreground">
                      {balance.remaining < 0 ? "超额" : "余额"}
                    </div>
                    <div
                      className={`mt-0.5 text-xl font-semibold tabular-nums ${
                        balance.remaining < 0
                          ? "text-red-600 dark:text-red-400"
                          : balance.remaining === 0
                            ? "text-amber-600 dark:text-amber-400"
                            : "text-emerald-600 dark:text-emerald-400"
                      }`}
                    >
                      {balance.remaining < 0
                        ? `-$${formatNumber(Math.abs(balance.remaining))}`
                        : `$${formatNumber(balance.remaining)}`}
                    </div>
                  </div>
                  <div className="text-right min-w-0">
                    <div className="text-[11px] uppercase tracking-wider text-muted-foreground">
                      超额
                    </div>
                    <div className="mt-1 flex items-center justify-end">
                      <OverageStatusPill balance={balance} />
                    </div>
                  </div>
                </div>
                <div className="space-y-1.5">
                  <Progress value={balance.usagePercentage} />
                  <div className="flex justify-between text-[11px] tabular-nums text-muted-foreground">
                    <span>已用 ${formatNumber(balance.currentUsage)}</span>
                    <span>{balance.usagePercentage.toFixed(1)}%</span>
                    <span>额度 ${formatNumber(balance.usageLimit)}</span>
                  </div>
                </div>
                <div className="border-t border-border/50 pt-2 text-[11px] text-muted-foreground">
                  下次重置：
                  <span className="font-medium text-foreground">
                    {formatResetDate(balance.nextResetAt)}
                  </span>
                </div>
              </div>
            ) : (
              <div className="py-2 text-[13px] text-muted-foreground">
                余额未查询，点击顶部"刷新当前页余额"即可加载。
              </div>
            )}
          </div>

          {/* 操作区 */}
          <div className="flex items-center justify-between gap-2 border-t border-border/50 pt-3">
            <div className="flex items-center gap-1">
              <Button
                size="icon"
                variant="ghost"
                onClick={() => {
                  const np = Math.max(0, credential.priority - 1);
                  setPriority.mutate(
                    { id: credential.id, priority: np },
                    {
                      onSuccess: (res) => toast.success(res.message),
                      onError: (err) =>
                        toast.error("操作失败: " + (err as Error).message),
                    },
                  );
                }}
                disabled={setPriority.isPending || credential.priority === 0}
                title="提高优先级"
              >
                <ChevronUp className="h-4 w-4" />
              </Button>
              <Button
                size="icon"
                variant="ghost"
                onClick={() => {
                  const np = credential.priority + 1;
                  setPriority.mutate(
                    { id: credential.id, priority: np },
                    {
                      onSuccess: (res) => toast.success(res.message),
                      onError: (err) =>
                        toast.error("操作失败: " + (err as Error).message),
                    },
                  );
                }}
                disabled={setPriority.isPending}
                title="降低优先级"
              >
                <ChevronDown className="h-4 w-4" />
              </Button>
              <span className="mx-1 h-5 w-px bg-border/70" />
              <Button
                size="sm"
                variant="ghost"
                onClick={handleForceRefresh}
                disabled={
                  forceRefresh.isPending ||
                  credential.disabled ||
                  credential.authMethod === "api_key"
                }
                title={
                  credential.authMethod === "api_key"
                    ? "API Key 无需刷新"
                    : credential.disabled
                      ? "已禁用"
                      : "强制刷新 Token"
                }
              >
                <RefreshCw
                  className={`h-3.5 w-3.5 ${forceRefresh.isPending ? "animate-spin" : ""}`}
                />
                刷新 Token
              </Button>
              <Button
                size="sm"
                variant="ghost"
                onClick={onRefreshBalance}
                disabled={loadingBalance || credential.disabled}
                title={credential.disabled ? "已禁用" : "刷新余额"}
              >
                <RefreshCw
                  className={`h-3.5 w-3.5 ${loadingBalance ? "animate-spin" : ""}`}
                />
                刷新余额
              </Button>
            </div>

            <div className="flex items-center gap-1">
              <Button
                size="sm"
                variant="outline"
                onClick={() => setShowEditDialog(true)}
              >
                <Pencil className="h-3.5 w-3.5" />
                编辑
              </Button>
              <DropdownMenu>
                <DropdownMenuTrigger asChild>
                  <Button size="icon" variant="ghost" title="更多操作">
                    <MoreHorizontal className="h-4 w-4" />
                  </Button>
                </DropdownMenuTrigger>
                <DropdownMenuContent align="end">
                  <DropdownMenuItem
                    onSelect={(e) => {
                      e.preventDefault();
                      handleReset();
                    }}
                    disabled={
                      resetFailure.isPending ||
                      (credential.failureCount === 0 &&
                        credential.refreshFailureCount === 0)
                    }
                  >
                    <RotateCcw />
                    重置失败计数
                  </DropdownMenuItem>
                  {throttleRemaining > 0 && (
                    <DropdownMenuItem
                      onSelect={(e) => {
                        e.preventDefault();
                        handleClearThrottle();
                      }}
                      disabled={clearThrottle.isPending}
                    >
                      <Clock />
                      解除风控冷却（{formatThrottleCountdown(throttleRemaining)}
                      ）
                    </DropdownMenuItem>
                  )}
                  {balance?.overageCapable === true &&
                    (balance.overageEnabled ? (
                      <DropdownMenuItem
                        onSelect={(e) => {
                          e.preventDefault();
                          handleSetOverage(false);
                        }}
                        disabled={overageBusy}
                      >
                        <ZapOff />
                        关闭超额
                      </DropdownMenuItem>
                    ) : (
                      <DropdownMenuItem
                        onSelect={(e) => {
                          e.preventDefault();
                          handleSetOverage(true);
                        }}
                        disabled={overageBusy}
                      >
                        <Zap className="text-emerald-500" />
                        开启超额
                      </DropdownMenuItem>
                    ))}
                  {credential.authMethod !== "api_key" && (
                    <DropdownMenuSeparator />
                  )}
                  {credential.authMethod !== "api_key" && (
                    <DropdownMenuItem
                      onSelect={() => setShowReloginDialog(true)}
                    >
                      <LogIn />
                      重新登录
                    </DropdownMenuItem>
                  )}
                  {credential.authMethod !== "api_key" && (
                    <DropdownMenuItem
                      onSelect={() => setShowUpdateTokenDialog(true)}
                    >
                      <RefreshCw />
                      重新导入 Token
                    </DropdownMenuItem>
                  )}
                  <DropdownMenuSeparator />
                  <DropdownMenuItem
                    destructive
                    disabled={!credential.disabled}
                    onSelect={(e) => {
                      e.preventDefault();
                      if (!credential.disabled) {
                        toast.error("请先禁用凭据再删除");
                        return;
                      }
                      setShowDeleteDialog(true);
                    }}
                  >
                    <Trash2 />
                    删除凭据
                  </DropdownMenuItem>
                </DropdownMenuContent>
              </DropdownMenu>
            </div>
          </div>
        </CardContent>
      </Card>

      <Dialog open={showDeleteDialog} onOpenChange={setShowDeleteDialog}>
        <DialogContent className="sm:max-w-sm">
          <DialogHeader>
            <DialogTitle>确认删除凭据</DialogTitle>
            <DialogDescription>
              您确定要删除凭据 #{credential.id} 吗？此操作无法撤销。
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button
              variant="outline"
              onClick={() => setShowDeleteDialog(false)}
              disabled={deleteCredential.isPending}
            >
              取消
            </Button>
            <Button
              variant="destructive"
              onClick={handleDelete}
              disabled={deleteCredential.isPending || !credential.disabled}
            >
              确认删除
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <EditCredentialDialog
        open={showEditDialog}
        onOpenChange={setShowEditDialog}
        credential={credential}
      />
      <UpdateTokenDialog
        open={showUpdateTokenDialog}
        onOpenChange={setShowUpdateTokenDialog}
        credential={credential}
      />
      <ReloginDialog
        open={showReloginDialog}
        onOpenChange={setShowReloginDialog}
        credential={credential}
      />
    </>
  );
}
