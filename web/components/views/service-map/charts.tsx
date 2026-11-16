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

import { useEffect, useRef } from "react";
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
}: {
  option: EChartsCoreOption;
  height: number;
  testId?: string;
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

  return <div ref={element} style={{ height }} className="w-full" data-testid={testId} />;
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
}

/** A time series with a clock axis: the default shape for "over the window". */
export function TimeChart({
  series,
  format,
  target,
  yMax,
  height = 148,
  testId,
}: TimeChartOptions) {
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
      showSymbol: false,
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

  return <Chart option={option} height={height} testId={testId} />;
}

export interface BarChartOptions {
  items: Array<{ name: string; value: number }>;
  format: (value: number) => string;
  height?: number;
  color?: string;
  testId?: string;
}

/** A sorted horizontal ranking, largest at the top. */
export function RankingChart({ items, format, height = 180, color = "#6ba7f7", testId }: BarChartOptions) {
  const shown = items.slice(0, 8);
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
  return <Chart option={option} height={height} testId={testId} />;
}
