"use client";

// Chart primitives for the node panel.
//
// ECharts is loaded piecemeal (`echarts/core` + the two series types the panel
// uses) because the dashboard already pays for the graph renderer elsewhere:
// importing the full bundle here would double the JavaScript for one drawer.
//
// The shapes follow what the data means:
//
//   time series  a line with an area fill — throughput, latency, replica count
//                over the window; a step line where the value is a gauge;
//   bars         a ranking — top functions, latency per nameserver, peers by
//                rate. Sorted, one colour, value labels on the bars.
//
// Both take a formatter rather than a unit string so the axis, the tooltip and
// the labels agree on how a number reads (bytes, cores, %, ms).

import { useEffect, useId, useRef } from "react";
import * as echarts from "echarts/core";
import { BarChart, LineChart } from "echarts/charts";
import {
  GridComponent,
  LegendComponent,
  MarkLineComponent,
  TooltipComponent,
} from "echarts/components";
import { CanvasRenderer } from "echarts/renderers";
import type { EChartsCoreOption } from "echarts/core";

echarts.use([LineChart, BarChart, GridComponent, TooltipComponent, LegendComponent, MarkLineComponent, CanvasRenderer]);

const THEME = {
  axis: "#7f8a9c",
  split: "rgba(35,43,54,0.9)",
  surface: "#151a21",
  border: "#232b36",
  text: "#e7eaf0",
};

export function Chart({
  option,
  height,
  testId,
  label,
  describedBy,
}: {
  option: EChartsCoreOption;
  height: number;
  testId?: string;
  label?: string;
  describedBy?: string;
}) {
  const element = useRef<HTMLDivElement | null>(null);
  const instance = useRef<echarts.ECharts | null>(null);

  useEffect(() => {
    const node = element.current;
    if (!node) return;
    const chart = echarts.init(node, undefined, { renderer: "canvas" });
    instance.current = chart;
    const observer = new ResizeObserver(() => chart.resize());
    observer.observe(node);
    return () => {
      observer.disconnect();
      chart.dispose();
      instance.current = null;
    };
  }, []);

  useEffect(() => {
    instance.current?.setOption(option, { notMerge: true });
  }, [option]);

  return (
    <div
      ref={element}
      style={{ height }}
      className="w-full"
      data-testid={testId}
      role="img"
      aria-label={label}
      aria-describedby={describedBy}
    />
  );
}

/**
 * A chart is a picture, so the numbers behind it also get a real table.
 * `<details>` keeps the panel compact while leaving every value available to
 * keyboard and screen-reader users — and to anyone who wants to copy a value
 * out instead of reading it off a tooltip.
 */
function DataTable({
  columns,
  rows,
  testId,
}: {
  columns: string[];
  rows: string[][];
  testId?: string;
}) {
  if (!rows.length) return null;
  return (
    <details className="mt-1 text-[10px] text-muted-foreground" data-testid={testId ? `${testId}-table` : undefined}>
      <summary className="w-fit cursor-pointer rounded focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring">
        View data table
      </summary>
      <div className="mt-1 max-h-32 overflow-auto rounded border">
        <table className="w-full border-collapse text-left font-mono text-[9px]">
          <thead className="sticky top-0 bg-muted/80">
            <tr>
              {columns.map((column) => (
                <th key={column} scope="col" className="border-b px-1.5 py-1 font-medium">
                  {column}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {rows.map((row, index) => (
              <tr key={row[0] ?? index} className="border-b last:border-0">
                {row.map((cell, cellIndex) => (
                  <td key={cellIndex} className="px-1.5 py-0.5">
                    {cell}
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </details>
  );
}

function clock(ms: number): string {
  const date = new Date(ms);
  const pad = (value: number) => String(value).padStart(2, "0");
  return `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`;
}

export interface ChartSeries {
  name: string;
  color: string;
  /** `[milliseconds, value]`, sparse where nothing was reported. */
  points: Array<[number, number]>;
  area?: boolean;
  step?: boolean;
  dashed?: boolean;
}

function tooltipBase() {
  return {
    confine: true,
    backgroundColor: THEME.surface,
    borderColor: THEME.border,
    borderWidth: 1,
    padding: [6, 8],
    textStyle: { color: THEME.text, fontSize: 11 },
    extraCssText: "box-shadow: 0 10px 24px -12px rgba(0,0,0,0.9); border-radius: 6px;",
  };
}

function axisBase(format: (value: number) => string) {
  return {
    type: "value" as const,
    axisLabel: {
      color: THEME.axis,
      fontSize: 9,
      formatter: (value: number) => format(value),
    },
    splitLine: { lineStyle: { color: THEME.split, type: "dashed" as const } },
    axisLine: { show: false },
    axisTick: { show: false },
  };
}

export interface TimeChartOptions {
  series: ChartSeries[];
  /** How a value reads on the axis and in the tooltip. */
  format: (value: number) => string;
  /** A dashed horizontal reference: an SLO target, for instance. */
  target?: { value: number; label: string };
  /** Fixed top of the y axis, for values that have a natural ceiling. */
  yMax?: number;
  height?: number;
  testId?: string;
  /** What the line means, for assistive technology. */
  description?: string;
}

/** A time series with a clock axis: the default shape for "over the window". */
export function TimeChart({
  series,
  format,
  target,
  yMax,
  height = 148,
  testId,
  description,
}: TimeChartOptions) {
  const descriptionId = useId();
  const label = description ?? `Time series with ${series.length} series over the selected window`;
  const option: EChartsCoreOption = {
    backgroundColor: "transparent",
    animationDuration: 320,
    grid: { left: 46, right: 12, top: target ? 26 : 12, bottom: 22 },
    tooltip: {
      trigger: "axis",
      ...tooltipBase(),
      axisPointer: { type: "line", lineStyle: { color: THEME.axis, width: 0.8, type: "dashed" } },
      formatter: (params: unknown) => {
        const rows = params as Array<{ value?: [number, number]; seriesName?: string; color?: string }>;
        if (!rows?.length) return "";
        const clock = rows[0]?.value?.[0] ?? 0;
        const lines = [
          `<div style="color:${THEME.axis};margin-bottom:2px">${new Date(clock).toLocaleTimeString()}</div>`,
        ];
        for (const row of rows) {
          const value = row.value?.[1];
          lines.push(
            `<div style="display:flex;gap:8px;justify-content:space-between">` +
              `<span><span style="display:inline-block;width:7px;height:7px;border-radius:2px;background:${row.color};margin-right:5px"></span>${row.seriesName}</span>` +
              `<span style="font-family:ui-monospace,monospace">${value === undefined ? "–" : format(value)}</span></div>`,
          );
        }
        return lines.join("");
      },
    },
    legend:
      series.length > 1
        ? {
            top: 0,
            right: 0,
            itemWidth: 7,
            itemHeight: 7,
            icon: "roundRect",
            textStyle: { color: THEME.axis, fontSize: 9 },
          }
        : { show: false },
    xAxis: {
      type: "time",
      axisLabel: {
        color: THEME.axis,
        fontSize: 9,
        hideOverlap: true,
        formatter: (value: number) => {
          const date = new Date(value);
          const pad = (n: number) => String(n).padStart(2, "0");
          return `${pad(date.getHours())}:${pad(date.getMinutes())}`;
        },
      },
      axisLine: { lineStyle: { color: THEME.split } },
      axisTick: { show: false },
      splitLine: { show: false },
    },
    yAxis: { ...axisBase(format), max: yMax, minInterval: yMax ? 1 : undefined },
    series: series.map((entry) => ({
      type: "line" as const,
      name: entry.name,
      // A one-bucket window has a value but no line. Showing the symbol makes
      // the single observation visible instead of looking like an empty chart.
      showSymbol: entry.points.length <= 2,
      symbol: "circle",
      symbolSize: 4,
      step: entry.step ? ("end" as const) : undefined,
      lineStyle: {
        color: entry.color,
        width: 1.4,
        type: entry.dashed ? ("dashed" as const) : ("solid" as const),
      },
      itemStyle: { color: entry.color },
      areaStyle: entry.area
        ? {
            color: new echarts.graphic.LinearGradient(0, 0, 0, 1, [
              { offset: 0, color: `${entry.color}55` },
              { offset: 1, color: `${entry.color}05` },
            ]),
          }
        : undefined,
      data: entry.points,
      markLine: target
        ? {
            silent: true,
            symbol: "none",
            label: {
              formatter: target.label,
              color: THEME.axis,
              fontSize: 9,
              position: "insideEndTop",
            },
            lineStyle: { color: "#f2a65a", type: "dashed" as const, width: 1 },
            data: [{ yAxis: target.value }],
          }
        : undefined,
    })),
  };

  const tableRows = series[0]?.points.map(([time], index) => [
    clock(time),
    ...series.map((entry) => (entry.points[index] ? format(entry.points[index][1]) : "—")),
  ]) ?? [];

  return (
    <>
      <p id={descriptionId} className="sr-only">
        {label}
      </p>
      <Chart
        option={option}
        height={height}
        testId={testId}
        label={label}
        describedBy={descriptionId}
      />
      <DataTable
        columns={["Time", ...series.map((entry) => entry.name)]}
        rows={tableRows}
        testId={testId}
      />
    </>
  );
}

export interface BarChartOptions {
  items: Array<{ name: string; value: number }>;
  format: (value: number) => string;
  height?: number;
  color?: string;
  testId?: string;
  /** Rankings default to eight; the profile tab asks for its full top 12. */
  limit?: number;
  description?: string;
}

/** A sorted horizontal ranking, largest at the top. */
export function RankingChart({
  items,
  format,
  height = 180,
  color = "#6ba7f7",
  testId,
  limit = 8,
  description,
}: BarChartOptions) {
  const descriptionId = useId();
  const shown = items.slice(0, limit);
  const label =
    description ??
    `Ranking of ${shown.length} items, largest first`;
  const option: EChartsCoreOption = {
    backgroundColor: "transparent",
    animationDuration: 320,
    grid: { left: 4, right: 52, top: 4, bottom: 4, containLabel: true },
    tooltip: {
      trigger: "item",
      ...tooltipBase(),
      formatter: (params: unknown) => {
        const row = params as { name?: string; value?: number };
        return `${row.name ?? ""}<br/><b>${format(row.value ?? 0)}</b>`;
      },
    },
    xAxis: { type: "value", show: false },
    yAxis: {
      type: "category",
      inverse: true,
      data: shown.map((item) => item.name),
      axisLabel: { color: THEME.axis, fontSize: 9, width: 118, overflow: "truncate" },
      axisLine: { show: false },
      axisTick: { show: false },
    },
    series: [
      {
        type: "bar",
        barWidth: 9,
        itemStyle: { color, borderRadius: [0, 3, 3, 0] },
        label: {
          show: true,
          position: "right",
          color: THEME.text,
          fontSize: 9,
          formatter: (params: unknown) => format((params as { value?: number }).value ?? 0),
        },
        data: shown.map((item) => item.value),
      },
    ],
  };
  return (
    <>
      <p id={descriptionId} className="sr-only">
        {label}
      </p>
      <Chart
        option={option}
        height={height}
        testId={testId}
        label={label}
        describedBy={descriptionId}
      />
      <DataTable
        columns={["Item", "Value"]}
        rows={shown.map((item) => [item.name, format(item.value)])}
        testId={testId}
      />
    </>
  );
}
