import { useEffect, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { DollarSign, RefreshCw, Save, ShieldAlert, TriangleAlert } from 'lucide-react'
import { toast } from 'sonner'

import {
  getProfitConfig,
  runProfitReport,
  updateProfitConfig,
} from '@/api/profit'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from '@/components/ui/card'
import { Input } from '@/components/ui/input'
import { cn, extractErrorMessage } from '@/lib/utils'
import type { ProfitBreakdownStat, ProfitReport } from '@/types/api'

const DEFAULT_CREDIT_PRICE = 0.0225
const TIME_RANGES = [
  { label: '30 分钟', minutes: 30 },
  { label: '2 小时', minutes: 120 },
  { label: '24 小时', minutes: 1_440 },
  { label: '7 天', minutes: 10_080 },
] as const

type Breakdown = 'group' | 'key' | 'model' | 'user'

export function ProfitPage() {
  const queryClient = useQueryClient()
  const configQuery = useQuery({
    queryKey: ['profit-config'],
    queryFn: getProfitConfig,
  })
  const [newapiBase, setNewapiBase] = useState('')
  const [newapiUser, setNewapiUser] = useState('')
  const [newapiToken, setNewapiToken] = useState('')
  const [creditPrice, setCreditPrice] = useState(DEFAULT_CREDIT_PRICE)
  const [quotaPerUnit, setQuotaPerUnit] = useState(500_000)
  const [minutes, setMinutes] = useState(120)
  const [report, setReport] = useState<ProfitReport | null>(null)
  const [breakdown, setBreakdown] = useState<Breakdown>('group')

  useEffect(() => {
    const config = configQuery.data
    if (!config) return
    setNewapiBase(config.newapiBase ?? '')
    setNewapiUser(config.newapiUser ?? '')
    setCreditPrice(config.creditPrice)
    setQuotaPerUnit(config.quotaPerUnit)
  }, [configQuery.data])

  const saveMutation = useMutation({
    mutationFn: updateProfitConfig,
    onSuccess: async () => {
      setNewapiToken('')
      await queryClient.invalidateQueries({ queryKey: ['profit-config'] })
      toast.success('利润配置已保存')
    },
    onError: (error) => toast.error(extractErrorMessage(error)),
  })
  const reportMutation = useMutation({
    mutationFn: runProfitReport,
    onSuccess: setReport,
    onError: (error) => toast.error(extractErrorMessage(error)),
  })

  const save = () => {
    saveMutation.mutate({
      newapiBase: newapiBase.trim(),
      newapiUser: newapiUser.trim(),
      newapiToken,
      creditPrice,
      quotaPerUnit,
    })
  }
  const selectRange = (value: number) => {
    setMinutes(value)
    reportMutation.mutate(value)
  }

  return (
    <div className="space-y-5">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h2 className="flex items-center gap-2 text-lg font-semibold">
            <DollarSign className="h-4 w-4" />
            NewAPI 利润
          </h2>
          <p className="mt-1 text-sm text-muted-foreground">
            收入按已识别的 RS 渠道统计；顶部总成本来自 RS 实际 metering 账本。
          </p>
        </div>
        <Badge variant={configQuery.data?.tokenConfigured ? 'success' : 'warning'}>
          {configQuery.data?.tokenConfigured ? 'Token 已配置' : 'Token 未配置'}
        </Badge>
      </div>

      <ConfigCard
        newapiBase={newapiBase}
        newapiUser={newapiUser}
        newapiToken={newapiToken}
        creditPrice={creditPrice}
        quotaPerUnit={quotaPerUnit}
        tokenConfigured={configQuery.data?.tokenConfigured ?? false}
        pending={saveMutation.isPending || configQuery.isLoading}
        onBaseChange={setNewapiBase}
        onUserChange={setNewapiUser}
        onTokenChange={setNewapiToken}
        onCreditPriceChange={setCreditPrice}
        onQuotaPerUnitChange={setQuotaPerUnit}
        onSave={save}
      />

      <Card>
        <CardHeader className="gap-3 sm:flex-row sm:items-center sm:justify-between">
          <div>
            <CardTitle>利润报表</CardTitle>
            <CardDescription>总成本使用 usage 账本，分组表仅展示可精确归属部分。</CardDescription>
          </div>
          <div className="flex flex-wrap gap-2">
            {TIME_RANGES.map((range) => (
              <Button
                key={range.minutes}
                size="sm"
                variant={minutes === range.minutes ? 'default' : 'outline'}
                disabled={reportMutation.isPending}
                onClick={() => selectRange(range.minutes)}
              >
                {range.label}
              </Button>
            ))}
            <Button
              size="sm"
              variant="outline"
              disabled={reportMutation.isPending}
              onClick={() => reportMutation.mutate(minutes)}
            >
              <RefreshCw className={cn('h-3.5 w-3.5', reportMutation.isPending && 'animate-spin')} />
              刷新
            </Button>
          </div>
        </CardHeader>
        <CardContent className="space-y-4">
          {report ? (
            <>
              <KpiGrid report={report} />
              <ReportWarnings report={report} />
              <BreakdownTable
                breakdown={breakdown}
                report={report}
                onBreakdownChange={setBreakdown}
              />
            </>
          ) : (
            <div className="rounded-xl border border-dashed px-4 py-12 text-center text-sm text-muted-foreground">
              选择时间范围即可生成报表。
            </div>
          )}
        </CardContent>
      </Card>
    </div>
  )
}

interface ConfigCardProps {
  newapiBase: string
  newapiUser: string
  newapiToken: string
  creditPrice: number
  quotaPerUnit: number
  tokenConfigured: boolean
  pending: boolean
  onBaseChange: (value: string) => void
  onUserChange: (value: string) => void
  onTokenChange: (value: string) => void
  onCreditPriceChange: (value: number) => void
  onQuotaPerUnitChange: (value: number) => void
  onSave: () => void
}

function ConfigCard(props: ConfigCardProps) {
  return (
    <Card>
      <CardHeader>
        <CardTitle>连接与成本配置</CardTitle>
        <CardDescription>
          Token 永不回显；输入框留空会保留已保存的 Token。
        </CardDescription>
      </CardHeader>
      <CardContent className="grid gap-4 md:grid-cols-2 xl:grid-cols-5">
        <Field label="NewAPI 地址" className="xl:col-span-2">
          <Input
            value={props.newapiBase}
            placeholder="https://newapi.example.com"
            disabled={props.pending}
            onChange={(event) => props.onBaseChange(event.target.value)}
          />
        </Field>
        <Field label="管理员用户 ID">
          <Input
            value={props.newapiUser}
            placeholder="1"
            disabled={props.pending}
            onChange={(event) => props.onUserChange(event.target.value)}
          />
        </Field>
        <Field label="管理员 Token">
          <Input
            type="password"
            value={props.newapiToken}
            placeholder={props.tokenConfigured ? '留空保留现有 Token' : '请输入 Token'}
            disabled={props.pending}
            onChange={(event) => props.onTokenChange(event.target.value)}
          />
        </Field>
        <div className="flex items-end">
          <Button className="w-full" disabled={props.pending} onClick={props.onSave}>
            <Save className="h-3.5 w-3.5" />
            {props.pending ? '保存中…' : '保存配置'}
          </Button>
        </div>
        <Field label="每 Credit 成本（¥）">
          <Input
            type="number"
            min="0.000001"
            step="0.0001"
            value={props.creditPrice}
            disabled={props.pending}
            onChange={(event) => props.onCreditPriceChange(Number(event.target.value))}
          />
          <p className="text-[11px] text-muted-foreground">默认 45 / 2000 = 0.0225</p>
        </Field>
        <Field label="每 1 元 quota">
          <Input
            type="number"
            min="1"
            step="1"
            value={props.quotaPerUnit}
            disabled={props.pending}
            onChange={(event) => props.onQuotaPerUnitChange(Number(event.target.value))}
          />
        </Field>
      </CardContent>
    </Card>
  )
}

function Field({
  children,
  className,
  label,
}: {
  children: React.ReactNode
  className?: string
  label: string
}) {
  return (
    <label className={cn('space-y-1.5 text-sm font-medium', className)}>
      <span>{label}</span>
      {children}
    </label>
  )
}

function KpiGrid({ report }: { report: ProfitReport }) {
  const attributionPct = report.revenue > 0
    ? report.attributedRevenue / report.revenue * 100
    : 0
  const cards = [
    ['收入', money(report.revenue)],
    ['上游 Credits', number(report.credits, 4)],
    ['成本', money(report.cost)],
    ['利润', report.ledgerScopeConfirmed ? money(report.profit) : '不可用'],
    ['毛利率', report.ledgerScopeConfirmed ? `${number(report.marginPct, 2)}%` : '不可用'],
    ['归属率', `${number(attributionPct, 2)}%`],
  ]
  return (
    <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-6">
      {cards.map(([label, value]) => (
        <div key={label} className="rounded-xl border border-border/60 bg-muted/20 p-3">
          <p className="text-xs text-muted-foreground">{label}</p>
          <p
            className={cn(
              'mt-1 text-lg font-semibold tabular-nums',
              (label === '利润' && report.ledgerScopeConfirmed && report.profit < 0) && 'text-destructive',
            )}
          >
            {value}
          </p>
        </div>
      ))}
    </div>
  )
}

function ReportWarnings({ report }: { report: ProfitReport }) {
  const hasUnattributed = report.unattributedRevenue > 0 || report.unattributedCredits > 0 || report.unattributedCost > 0
  if (report.ledgerScopeConfirmed && report.unmatched === 0 && report.missingCost === 0 && !hasUnattributed) return null
  return (
    <div
      role="alert"
      aria-live="polite"
      className={cn(
        'flex flex-wrap items-center gap-2 rounded-xl border p-3 text-xs',
        report.ledgerScopeConfirmed
          ? 'border-amber-500/30 bg-amber-500/5'
          : 'border-destructive/40 bg-destructive/5 text-destructive',
      )}
    >
      {report.ledgerScopeConfirmed ? (
        <TriangleAlert className="h-4 w-4 text-amber-600" />
      ) : (
        <ShieldAlert className="h-4 w-4" />
      )}
      {!report.ledgerScopeConfirmed && <strong>范围未确认：暂不展示可信利润结论。</strong>}
      {report.unmatched > 0 && (
        <span>未匹配收入：{report.unmatched} 条 / {money(report.unmatchedRevenue)}</span>
      )}
      {report.unattributedRevenue > 0 && <span>未归属收入：{money(report.unattributedRevenue)}</span>}
      {report.unattributedCredits > 0 && <span>未归属 Credits：{number(report.unattributedCredits, 4)}</span>}
      {report.unattributedCost > 0 && <span>未归属成本：{money(report.unattributedCost)}</span>}
      {report.missingCost > 0 && <span>缺失成本记录：{report.missingCost} 条</span>}
      <span className={report.ledgerScopeConfirmed ? 'text-muted-foreground' : undefined}>
        顶部总成本来自 RS 实际 metering 账本；分组表仅展示可精确归属部分。
      </span>
    </div>
  )
}

function BreakdownTable({
  breakdown,
  onBreakdownChange,
  report,
}: {
  breakdown: Breakdown
  onBreakdownChange: (value: Breakdown) => void
  report: ProfitReport
}) {
  const options: { label: string; value: Breakdown; rows: ProfitBreakdownStat[] }[] = [
    { label: '按分组', value: 'group', rows: report.byGroup },
    { label: '按 Key', value: 'key', rows: report.byKey },
    { label: '按模型', value: 'model', rows: report.byModel },
    { label: '按用户', value: 'user', rows: report.byUser },
  ]
  const rows = options.find((option) => option.value === breakdown)?.rows ?? []
  return (
    <div className="space-y-3">
      <div className="flex flex-wrap gap-2">
        {options.map((option) => (
          <Button
            key={option.value}
            size="sm"
            variant={breakdown === option.value ? 'secondary' : 'ghost'}
            onClick={() => onBreakdownChange(option.value)}
          >
            {option.label}
          </Button>
        ))}
      </div>
      <div className="overflow-x-auto rounded-xl border border-border/60">
        <table className="w-full min-w-[720px] text-sm">
          <thead className="bg-muted/40 text-left text-xs text-muted-foreground">
            <tr>
              <th className="px-3 py-2 font-medium">名称</th>
              <th className="px-3 py-2 text-right font-medium">请求</th>
              <th className="px-3 py-2 text-right font-medium">收入</th>
              <th className="px-3 py-2 text-right font-medium">Credits</th>
              <th className="px-3 py-2 text-right font-medium">成本</th>
              <th className="px-3 py-2 text-right font-medium">利润</th>
              <th className="px-3 py-2 text-right font-medium">缺失成本</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((row) => (
              <tr key={`${breakdown}-${row.name}-${row.keyId ?? ''}`} className="border-t border-border/50">
                <td className="max-w-[260px] truncate px-3 py-2 font-medium">{row.name}</td>
                <td className="px-3 py-2 text-right tabular-nums">{row.count}</td>
                <td className="px-3 py-2 text-right tabular-nums">{money(row.revenue)}</td>
                <td className="px-3 py-2 text-right tabular-nums">{number(row.credits, 4)}</td>
                <td className="px-3 py-2 text-right tabular-nums">{money(row.cost)}</td>
                <td className={cn('px-3 py-2 text-right tabular-nums', row.profit < 0 && 'text-destructive')}>
                  {money(row.profit)}
                </td>
                <td className="px-3 py-2 text-right tabular-nums">{row.missingCost}</td>
              </tr>
            ))}
            {rows.length === 0 && (
              <tr>
                <td className="px-3 py-8 text-center text-muted-foreground" colSpan={7}>暂无匹配数据</td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
    </div>
  )
}

function money(value: number): string {
  return `¥${number(value, 4)}`
}

function number(value: number, digits: number): string {
  return new Intl.NumberFormat('zh-CN', {
    maximumFractionDigits: digits,
    minimumFractionDigits: Math.min(2, digits),
  }).format(Number.isFinite(value) ? value : 0)
}
