// A Lightweight Charts v5 series primitive that fills the predicted-APY
// confidence band ([apy_low, apy_high]) behind the dashed projected line.
//
// Lightweight Charts has no native "fill between two series", so we draw the
// band ourselves: the primitive maps each point's (time, low/high) to pixel
// coordinates via the host series' price scale and the chart's time scale, then
// fills a ribbon polygon on the pane canvas beneath the line (zOrder "bottom").
// `autoscaleInfo` widens the price scale to include the band so a wide band
// never clips.

import type {
  AutoscaleInfo,
  Coordinate,
  IChartApi,
  IPrimitivePaneRenderer,
  IPrimitivePaneView,
  ISeriesApi,
  ISeriesPrimitive,
  SeriesAttachedParameter,
  SeriesType,
  Time,
} from "lightweight-charts";

export type BandPoint = { time: Time; low: number; high: number };

/** Minimal shape of the canvas target passed to a pane renderer's `draw`. */
interface BitmapTarget {
  useBitmapCoordinateSpace(
    cb: (scope: {
      context: CanvasRenderingContext2D;
      horizontalPixelRatio: number;
      verticalPixelRatio: number;
    }) => void,
  ): void;
}

class BandPaneRenderer implements IPrimitivePaneRenderer {
  constructor(private readonly _src: ApyBandPrimitive) {}

  draw(target: BitmapTarget): void {
    const chart = this._src.chart;
    const series = this._src.series;
    const data = this._src.data;
    if (!chart || !series || data.length < 2) return;
    const timeScale = chart.timeScale();

    const cols = data
      .map((d) => ({
        x: timeScale.timeToCoordinate(d.time),
        yHi: series.priceToCoordinate(d.high),
        yLo: series.priceToCoordinate(d.low),
      }))
      .filter(
        (c): c is { x: Coordinate; yHi: Coordinate; yLo: Coordinate } =>
          c.x !== null && c.yHi !== null && c.yLo !== null,
      );
    if (cols.length < 2) return;

    target.useBitmapCoordinateSpace(({ context: ctx, horizontalPixelRatio: hr, verticalPixelRatio: vr }) => {
      ctx.beginPath();
      ctx.moveTo(cols[0].x * hr, cols[0].yHi * vr);
      for (let i = 1; i < cols.length; i++) ctx.lineTo(cols[i].x * hr, cols[i].yHi * vr);
      for (let i = cols.length - 1; i >= 0; i--) ctx.lineTo(cols[i].x * hr, cols[i].yLo * vr);
      ctx.closePath();
      ctx.fillStyle = this._src.color;
      ctx.fill();
    });
  }
}

class BandPaneView implements IPrimitivePaneView {
  private readonly _renderer: BandPaneRenderer;
  constructor(src: ApyBandPrimitive) {
    this._renderer = new BandPaneRenderer(src);
  }
  // Draw beneath the series line so the dashed projection stays crisp on top.
  zOrder(): "bottom" {
    return "bottom";
  }
  renderer(): IPrimitivePaneRenderer {
    return this._renderer;
  }
}

export class ApyBandPrimitive implements ISeriesPrimitive<Time> {
  chart: IChartApi | null = null;
  series: ISeriesApi<SeriesType> | null = null;
  data: BandPoint[] = [];
  color = "rgba(47,129,247,0.16)";

  private readonly _paneViews: BandPaneView[];
  private _requestUpdate?: () => void;

  constructor() {
    this._paneViews = [new BandPaneView(this)];
  }

  attached(param: SeriesAttachedParameter<Time>): void {
    this.chart = param.chart;
    this.series = param.series;
    this._requestUpdate = param.requestUpdate;
  }

  detached(): void {
    this.chart = null;
    this.series = null;
    this._requestUpdate = undefined;
  }

  setData(data: BandPoint[], color: string): void {
    this.data = data;
    this.color = color;
    this._requestUpdate?.();
  }

  updateAllViews(): void {}

  paneViews(): readonly IPrimitivePaneView[] {
    return this._paneViews;
  }

  autoscaleInfo(): AutoscaleInfo | null {
    if (this.data.length === 0) return null;
    let min = Infinity;
    let max = -Infinity;
    for (const d of this.data) {
      if (d.low < min) min = d.low;
      if (d.high > max) max = d.high;
    }
    if (!Number.isFinite(min) || !Number.isFinite(max)) return null;
    return { priceRange: { minValue: min, maxValue: max } };
  }
}
