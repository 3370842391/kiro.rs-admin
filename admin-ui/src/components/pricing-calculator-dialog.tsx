import { useEffect, useState } from 'react'
import { useMutation, useQuery } from '@tanstack/react-query'
import { Calculator, TriangleAlert } from 'lucide-react'

import { getPricingCoefficients, simulatePricing } from '@/api/profit'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { Input } from '@/components/ui/input'
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'
import { cn, extractErrorMessage } from '@/lib/utils'
import type { PricingInput, PricingResult } from '@/types/api'

const BLENDED = '__blended__'

interface PricingCalculatorDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

export function PricingCalculatorDialog({
  open,
  onOpenChange,
}: PricingCalculatorDialogProps) {
  const coefficientsQuery = useQuery({
    queryKey: ['pricing-coefficients'],
    queryFn: getPricingCoefficients,
    enabled: open,
  })
  const [costRmb, setCostRmb] = useState('800')
  const [quotaCredits, setQuotaCredits] = useState('10000')
  const [groupRatio, setGroupRatio] = useState('0.3')
  const [targetMarginPct, setTargetMarginPct] = useState('40')
  const [consumedPct, setConsumedPct] = useState('100')
  const [model, setModel] = useState(BLENDED)
  const [result, setResult] = useState<PricingResult | null>(null)
  const [error, setError] = useState<string | null>(null)

  const simulateMutation = useMutation({
    mutationFn: simulatePricing,
    onSuccess: (data) => {
      setResult(data)
      setError(null)
    },
    onError: (err) => {
      setResult(null)
      setError(extractErrorMessage(err))
    },
  })

  useEffect(() => {
    if (!open) return
    const input = buildInput({
      costRmb,
      quotaCredits,
      groupRatio,
      targetMarginPct,
      consumedPct,
      model,
    })
    if (!input) {
      setResult(null)
      setError(null)
      return
    }
    const timer = window.setTimeout(() => {
      simulateMutation.mutate(input)
    }, 280)
    return () => window.clearTimeout(timer)
    // 只跟输入走：mutation 对象每次渲染都变，放进依赖会把自己打成死循环。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, costRmb, quotaCredits, groupRatio, targetMarginPct, consumedPct, model])

  const coefficients = coefficientsQuery.data
  const models = coefficients?.byModel ?? []

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-2xl">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Calculator className="h-4 w-4" />
            进价测算
          </DialogTitle>
          <DialogDescription>
            输入这批号多少钱、多少额度，算出该把 NewAPI 分组倍率设到多少，以及一个号能干多少 token。
          </DialogDescription>
        </DialogHeader>

        <CoefficientBanner
          loading={coefficientsQuery.isLoading}
          samples={coefficients?.samples ?? 0}
          measuredAt={coefficients?.measuredAt}
          rmbPerCreditRatio={coefficients?.rmbPerCreditRatio}
          tokensPerCredit={coefficients?.tokensPerCredit}
        />

        <div className="grid gap-3 sm:grid-cols-2">
          <Field label="买入价（¥）">
            <Input
              inputMode="decimal"
              value={costRmb}
              onChange={(event) => setCostRmb(event.target.value)}
            />
          </Field>
          <Field label="额度积分（credits）">
            <Input
              inputMode="decimal"
              value={quotaCredits}
              onChange={(event) => setQuotaCredits(event.target.value)}
            />
          </Field>
          <Field label="打算设的分组倍率" hint="正算：填了就给出单号收入 / 利润">
            <Input
              inputMode="decimal"
              value={groupRatio}
              placeholder="例如 0.3"
              onChange={(event) => setGroupRatio(event.target.value)}
            />
          </Field>
          <Field label="目标毛利率（%）" hint="反算：填了就给出该设的倍率">
            <Input
              inputMode="decimal"
              value={targetMarginPct}
              placeholder="例如 40"
              onChange={(event) => setTargetMarginPct(event.target.value)}
            />
          </Field>
          <Field label="额度能跑到 %" hint="线上号常在 87% 左右被封，按 100% 会高估收入">
            <Input
              inputMode="decimal"
              value={consumedPct}
              onChange={(event) => setConsumedPct(event.target.value)}
            />
          </Field>
          <Field label="模型口径">
            <Select value={model} onValueChange={setModel}>
              <SelectTrigger>
                <SelectValue placeholder="混合口径" />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value={BLENDED}>混合口径</SelectItem>
                {models.map((entry) => (
                  <SelectItem key={entry.model} value={entry.model}>
                    {entry.model}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </Field>
        </div>

        {error && (
          <div
            role="alert"
            className="rounded-xl border border-destructive/40 bg-destructive/5 px-3 py-2 text-xs text-destructive"
          >
            {error}
          </div>
        )}

        {result && <ResultGrid result={result} pending={simulateMutation.isPending} />}
      </DialogContent>
    </Dialog>
  )
}

function buildInput(raw: {
  costRmb: string
  quotaCredits: string
  groupRatio: string
  targetMarginPct: string
  consumedPct: string
  model: string
}): PricingInput | null {
  const costRmb = Number(raw.costRmb)
  const quotaCredits = Number(raw.quotaCredits)
  if (!(costRmb > 0) || !(quotaCredits > 0)) return null

  const input: PricingInput = { costRmb, quotaCredits }
  const groupRatio = optionalPositive(raw.groupRatio)
  const targetMarginPct = optionalNumber(raw.targetMarginPct)
  const consumedPct = optionalPositive(raw.consumedPct)
  if (groupRatio !== undefined) input.groupRatio = groupRatio
  if (targetMarginPct !== undefined) input.targetMarginPct = targetMarginPct
  if (consumedPct !== undefined) input.consumedPct = consumedPct
  if (raw.model && raw.model !== BLENDED) input.model = raw.model
  return input
}

function optionalPositive(raw: string): number | undefined {
  const trimmed = raw.trim()
  if (!trimmed) return undefined
  const value = Number(trimmed)
  return value > 0 ? value : undefined
}

function optionalNumber(raw: string): number | undefined {
  const trimmed = raw.trim()
  if (!trimmed) return undefined
  const value = Number(trimmed)
  return Number.isFinite(value) ? value : undefined
}

function CoefficientBanner({
  loading,
  measuredAt,
  rmbPerCreditRatio,
  samples,
  tokensPerCredit,
}: {
  loading: boolean
  measuredAt?: string
  rmbPerCreditRatio?: number
  samples: number
  tokensPerCredit?: number
}) {
  if (loading) {
    return (
      <div className="rounded-xl border border-dashed px-3 py-2 text-xs text-muted-foreground">
        正在读取实测系数…
      </div>
    )
  }
  if (rmbPerCreditRatio == null && tokensPerCredit == null) {
    return (
      <div className="flex items-start gap-2 rounded-xl border border-amber-500/30 bg-amber-500/5 px-3 py-2 text-xs">
        <TriangleAlert className="mt-0.5 h-3.5 w-3.5 text-amber-600" />
        <span>
          还没有实测系数，金额算不出来。请先在利润页跑一次报表，测算器会把卖价和 token 吞吐固化下来。
        </span>
      </div>
    )
  }
  return (
    <div className="flex flex-wrap items-center gap-2 rounded-xl border border-border/60 bg-muted/20 px-3 py-2 text-xs">
      <Badge variant="secondary">{samples} 条样本</Badge>
      {rmbPerCreditRatio != null && (
        <span className="tabular-nums text-muted-foreground">
          k = {number(rmbPerCreditRatio, 4)} ¥ / (credit · 倍率)
        </span>
      )}
      {tokensPerCredit != null && (
        <span className="tabular-nums text-muted-foreground">
          {compactTokens(tokensPerCredit)} token / credit
        </span>
      )}
      {measuredAt && (
        <span className="text-muted-foreground">
          {new Date(measuredAt).toLocaleString('zh-CN')}
        </span>
      )}
    </div>
  )
}

function ResultGrid({
  pending,
  result,
}: {
  pending: boolean
  result: PricingResult
}) {
  const cards: [string, string, boolean?][] = [
    ['成本 / credit', money(result.costPerCredit)],
    ['有效额度', number(result.effectiveCredits, 0)],
    ['回本倍率', formatOptional(result.breakevenGroupRatio, (value) => number(value, 3))],
    ['目标倍率', formatOptional(result.requiredGroupRatio, (value) => number(value, 3))],
    ['单号收入', formatOptional(result.revenueRmb, money)],
    ['单号利润', formatOptional(result.profitRmb, money), (result.profitRmb ?? 0) < 0],
    ['毛利率', formatOptional(result.marginPct, (value) => `${number(value, 2)}%`)],
    ['可产出 token', formatOptional(result.producibleTokens, compactTokens)],
  ]
  return (
    <div className={cn('space-y-3', pending && 'opacity-70')}>
      <div className="flex flex-wrap items-center gap-2">
        <p className="text-sm font-medium">测算结果</p>
        <Badge variant={result.modelExact ? 'success' : 'secondary'}>
          {result.modelExact ? '该模型精确实测' : '混合口径'}
        </Badge>
      </div>
      {result.warning && (
        <div
          role="alert"
          className="flex items-start gap-2 rounded-xl border border-destructive/40 bg-destructive/5 px-3 py-2 text-xs text-destructive"
        >
          <TriangleAlert className="mt-0.5 h-3.5 w-3.5" />
          <span>{result.warning}</span>
        </div>
      )}
      <div className="grid gap-2 sm:grid-cols-2 xl:grid-cols-4">
        {cards.map(([label, value, danger]) => (
          <div key={label} className="rounded-xl border border-border/60 bg-muted/20 p-3">
            <p className="text-xs text-muted-foreground">{label}</p>
            <p className={cn('mt-1 text-base font-semibold tabular-nums', danger && 'text-destructive')}>
              {value}
            </p>
          </div>
        ))}
      </div>
    </div>
  )
}

function Field({
  children,
  hint,
  label,
}: {
  children: React.ReactNode
  hint?: string
  label: string
}) {
  return (
    <label className="space-y-1.5 text-sm font-medium">
      <span>{label}</span>
      {children}
      {hint && <p className="text-[11px] font-normal text-muted-foreground">{hint}</p>}
    </label>
  )
}

function formatOptional(value: number | undefined, format: (value: number) => string): string {
  return value == null ? '—' : format(value)
}

function money(value: number): string {
  return `¥${number(value, 2)}`
}

function number(value: number, digits: number): string {
  return new Intl.NumberFormat('zh-CN', {
    maximumFractionDigits: digits,
    minimumFractionDigits: Math.min(2, digits),
  }).format(Number.isFinite(value) ? value : 0)
}

function compactTokens(value: number): string {
  if (!Number.isFinite(value)) return '—'
  if (Math.abs(value) >= 100_000_000) return `${number(value / 100_000_000, 2)} 亿`
  if (Math.abs(value) >= 10_000) return `${number(value / 10_000, 2)} 万`
  return number(value, 0)
}

export function PricingCalculatorButton({
  className,
  onClick,
}: {
  className?: string
  onClick: () => void
}) {
  return (
    <Button size="sm" variant="outline" className={className} onClick={onClick}>
      <Calculator className="h-3.5 w-3.5" />
      进价测算
    </Button>
  )
}
