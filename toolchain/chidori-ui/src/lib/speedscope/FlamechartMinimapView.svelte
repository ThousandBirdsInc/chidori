<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { Flamechart } from './lib/flamechart';
  import { Rect, Vec2, AffineTransform, clamp } from './lib/math';
  import { FlamechartRenderer } from './gl/flamechart-renderer';
  import { FontFamily, FontSize, Sizes } from './themes/styles';
  import { CanvasContext } from './gl/canvas-context';
  import { cachedMeasureTextWidth } from './lib/text-utils';
  import { Color } from './lib/color';
  import { type Theme } from './themes/theme';
  import {lightTheme} from "@/speedscope/themes/light-theme.js";

  export let flamechart: Flamechart = null;
  export let canvasContext: CanvasContext = null;
  export let flamechartRenderer: FlamechartRenderer = null;
  export let theme: Theme = lightTheme;
  export let configSpaceViewportRect: Rect = null;
  export let transformViewport: (transform: AffineTransform) => void;
  export let setConfigSpaceViewportRect: (rect: Rect) => void;

  let container: HTMLDivElement | null = null;
  let overlayCanvas: HTMLCanvasElement | null = null;
  let overlayCtx = null;

  const DraggingMode = {
    DRAW_NEW_VIEWPORT: 0,
    TRANSLATE_VIEWPORT: 1,
  };

  function physicalViewSize() {
    return new Vec2(
      overlayCanvas ? overlayCanvas.width : 0,
      overlayCanvas ? overlayCanvas.height : 0,
    );
  }

  function minimapOrigin() {
    return new Vec2(0, 0);
  }

  function configSpaceSize() {
    return new Vec2(
      flamechart.getTotalWeight(),
      flamechart.getLayers().length,
    );
  }

  function configSpaceToPhysicalViewSpace() {
    const origin = minimapOrigin();

    return AffineTransform.betweenRects(
      new Rect(new Vec2(0, 0), configSpaceSize()),
      new Rect(origin, physicalViewSize().minus(origin)),
    );
  }

  function logicalToPhysicalViewSpace() {
    return AffineTransform.withScale(new Vec2(window.devicePixelRatio, window.devicePixelRatio));
  }

  function windowToLogicalViewSpace() {
    if (!container) return new AffineTransform();
    const bounds = container.getBoundingClientRect();
    return AffineTransform.withTranslation(new Vec2(-bounds.left, -bounds.top));
  }

  function renderRects() {
    if (!container) return;

    // Hasn't resized yet -- no point in rendering yet
    if (physicalViewSize().x < 2) return;

    canvasContext.renderBehind(container, () => {
      flamechartRenderer.render({
        configSpaceSrcRect: new Rect(new Vec2(0, 0), configSpaceSize()),
        physicalSpaceDstRect: new Rect(
          minimapOrigin(),
          physicalViewSize().minus(minimapOrigin()),
        ),
        renderOutlines: false,
      });

      canvasContext.viewportRectangleRenderer.render({
        configSpaceViewportRect: configSpaceViewportRect,
        configSpaceToPhysicalViewSpace: configSpaceToPhysicalViewSpace(),
      });
    });
  }

  function renderOverlays() {
    overlayCtx = overlayCanvas.getContext('2d');
    const ctx = overlayCtx;
    if (!ctx) return;
    const viewSize = physicalViewSize();
    ctx.clearRect(0, 0, viewSize.x, viewSize.y);

    const configToPhysical = configSpaceToPhysicalViewSpace();

    const left = 0;
    const right = configSpaceSize().x;

    // TODO(jlfwong): There's a huge amount of code duplication here between
    // this and the FlamechartView.renderOverlays(). Consolidate.

    // We want about 10 gridlines to be visible, and want the unit to be
    // 1eN, 2eN, or 5eN for some N

    // Ideally, we want an interval every 100 logical screen pixels
    const logicalToConfig = (
      configSpaceToPhysicalViewSpace().inverted() || new AffineTransform()
    ).times(logicalToPhysicalViewSpace());

    const targetInterval = logicalToConfig.transformVector(new Vec2(200, 1)).x;

    const physicalViewSpaceFrameHeight = Sizes.FRAME_HEIGHT * window.devicePixelRatio;
    const physicalViewSpaceFontSize = FontSize.LABEL * window.devicePixelRatio;
    const labelPaddingPx = (physicalViewSpaceFrameHeight - physicalViewSpaceFontSize) / 2;

    ctx.font = `${physicalViewSpaceFontSize}px/${physicalViewSpaceFrameHeight}px ${FontFamily.MONOSPACE}`;
    ctx.textBaseline = 'top';

    const minInterval = Math.pow(10, Math.floor(Math.log10(targetInterval)));
    let interval = minInterval;

    if (targetInterval / interval > 5) {
      interval *= 5;
    } else if (targetInterval / interval > 2) {
      interval *= 2;
    }

    {
      ctx.fillStyle = Color.fromCSSHex(theme.bgPrimaryColor).withAlpha(0.8).toCSS();
      ctx.fillRect(0, 0, viewSize.x, physicalViewSpaceFrameHeight);
      ctx.textBaseline = 'top';

      for (let x = Math.ceil(left / interval) * interval; x < right; x += interval) {
        // TODO(jlfwong): Ensure that labels do not overlap
        const pos = Math.round(configToPhysical.transformPosition(new Vec2(x, 0)).x);
        const labelText = flamechart.formatValue(x);
        const textWidth = Math.ceil(cachedMeasureTextWidth(ctx, labelText));

        ctx.fillStyle = theme.fgPrimaryColor;
        ctx.fillText(labelText, pos - textWidth - labelPaddingPx, labelPaddingPx);
        ctx.fillStyle = theme.fgSecondaryColor;
        ctx.fillRect(pos, 0, 1, viewSize.y);
      }
    }
  }

  function onWindowResize() {
    onBeforeFrame();
  }

  function resizeOverlayCanvasIfNeeded() {
    if (!overlayCanvas) return;
    let { width, height } = overlayCanvas.getBoundingClientRect();
    {
      /*
      We render text at a higher resolution then scale down to
      ensure we're rendering at 1:1 device pixel ratio.
      This ensures our text is rendered crisply.
    */
    }
    width = Math.floor(width);
    height = Math.floor(height);

    // Still initializing: don't resize yet
    if (width === 0 || height === 0) return;

    const scaledWidth = width * window.devicePixelRatio;
    const scaledHeight = height * window.devicePixelRatio;

    if (scaledWidth === overlayCanvas.width && scaledHeight === overlayCanvas.height) return;

    overlayCanvas.width = scaledWidth;
    overlayCanvas.height = scaledHeight;
  }

  let frameHadWheelEvent = false;
  let framesWithoutWheelEvents = 0;
  let interactionLock: 'pan' | 'zoom' | null = null;
  function maybeClearInteractionLock() {
    if (interactionLock) {
      if (!frameHadWheelEvent) {
        framesWithoutWheelEvents++;
        if (framesWithoutWheelEvents >= 2) {
          interactionLock = null;
          framesWithoutWheelEvents = 0;
        }
      }
      canvasContext.requestFrame();
    }
    frameHadWheelEvent = false;
  }

  function pan(logicalViewSpaceDelta) {
    interactionLock = 'pan';
    const physicalDelta = logicalToPhysicalViewSpace().transformVector(logicalViewSpaceDelta);
    const configDelta = configSpaceToPhysicalViewSpace().inverseTransformVector(physicalDelta);

    if (!configDelta) return;
    transformViewport(AffineTransform.withTranslation(configDelta));
  }

  function zoom(multiplier) {
    interactionLock = 'zoom';
    const configSpaceCenter = configSpaceViewportRect.origin.plus(configSpaceViewportRect.size.times(1 / 2));
    if (!configSpaceCenter) return;

    const zoomTransform = AffineTransform.withTranslation(configSpaceCenter.times(-1))
      .scaledBy(new Vec2(multiplier, 1))
      .translatedBy(configSpaceCenter);

    transformViewport(zoomTransform);
  }

  function onWheel(ev) {
    ev.preventDefault();

    frameHadWheelEvent = true;

    const isZoom = ev.metaKey || ev.ctrlKey;

    if (isZoom && interactionLock !== 'pan') {
      let multiplier = 1 + ev.deltaY / 100;

      // On Chrome & Firefox, pinch-to-zoom maps to
      // WheelEvent + Ctrl Key. We'll accelerate it in
      // this case, since it feels a bit sluggish otherwise.
      if (ev.ctrlKey) {
        multiplier = 1 + ev.deltaY / 40;
      }

      multiplier = clamp(multiplier, 0.1, 10.0);

      zoom(multiplier);
    } else if (interactionLock !== 'zoom') {
      pan(new Vec2(ev.deltaX, ev.deltaY));
    }

    renderCanvas();
  }

  function configSpaceMouse(ev) {
    const logicalSpaceMouse = windowToLogicalViewSpace().transformPosition(
      new Vec2(ev.clientX, ev.clientY),
    );
    const physicalSpaceMouse = logicalToPhysicalViewSpace().transformPosition(
      logicalSpaceMouse,
    );
    return configSpaceToPhysicalViewSpace().inverseTransformPosition(physicalSpaceMouse);
  }

  let dragStartConfigSpaceMouse: Vec2 | null = null
  let dragConfigSpaceViewportOffset: Vec2 | null = null
  let draggingMode: DraggingMode | null = null

  function onMouseDown(ev) {
    const _configSpaceMouse = configSpaceMouse(ev);

    if (_configSpaceMouse) {
      if (configSpaceViewportRect.contains(_configSpaceMouse)) {
        // If dragging starting inside the viewport rectangle,
        // we'll move the existing viewport
        draggingMode = DraggingMode.TRANSLATE_VIEWPORT;
        dragConfigSpaceViewportOffset = _configSpaceMouse.minus(
          configSpaceViewportRect.origin,
        );
      } else {
        // If dragging starts outside the viewport rectangle,
        // we'll start drawing a new viewport
        draggingMode = DraggingMode.DRAW_NEW_VIEWPORT;
      }

      dragStartConfigSpaceMouse = _configSpaceMouse;
      window.addEventListener('mousemove', onWindowMouseMove);
      window.addEventListener('mouseup', onWindowMouseUp);
      updateCursor(_configSpaceMouse);
    }
  }

  function onWindowMouseMove(ev) {
    if (!dragStartConfigSpaceMouse) return;
    let _configSpaceMouse = configSpaceMouse(ev);

    if (!_configSpaceMouse) return;
    updateCursor(_configSpaceMouse);

    // Clamp the mouse position to avoid weird behavior when outside the canvas bounds
    _configSpaceMouse = new Rect(new Vec2(0, 0), configSpaceSize()).closestPointTo(
      _configSpaceMouse,
    );

    if (draggingMode === DraggingMode.DRAW_NEW_VIEWPORT) {
      const configStart = dragStartConfigSpaceMouse;
      let configEnd = _configSpaceMouse;

      if (!configStart || !configEnd) return;
      const left = Math.min(configStart.x, configEnd.x);
      const right = Math.max(configStart.x, configEnd.x);

      const width = right - left;
      const height = configSpaceViewportRect.height();

      setConfigSpaceViewportRect(
        new Rect(new Vec2(left, configEnd.y - height / 2), new Vec2(width, height)),
      );
    } else if (draggingMode === DraggingMode.TRANSLATE_VIEWPORT) {
      if (!dragConfigSpaceViewportOffset) return;

      const newOrigin = _configSpaceMouse.minus(dragConfigSpaceViewportOffset);
      setConfigSpaceViewportRect(
        configSpaceViewportRect.withOrigin(newOrigin),
      );
    }
  }

  function updateCursor(configSpaceMouse: Vec2) {
    if (draggingMode === DraggingMode.TRANSLATE_VIEWPORT) {
      document.body.style.cursor = 'grabbing';
      document.body.style.cursor = '-webkit-grabbing';
    } else if (draggingMode === DraggingMode.DRAW_NEW_VIEWPORT) {
      document.body.style.cursor = 'col-resize';
    } else if (configSpaceViewportRect.contains(configSpaceMouse)) {
      document.body.style.cursor = 'grab';
      document.body.style.cursor = '-webkit-grab';
    } else {
      document.body.style.cursor = 'col-resize';
    }
  }

  function onMouseLeave() {
    if (draggingMode == null) {
      document.body.style.cursor = 'default';
    }
  }

  function onMouseMove(ev) {
    const _configSpaceMouse = configSpaceMouse(ev);
    if (!_configSpaceMouse) return;
    updateCursor(_configSpaceMouse);
  }

  function onWindowMouseUp(ev) {
    draggingMode = null;
    window.removeEventListener('mousemove', onWindowMouseMove);
    window.removeEventListener('mouseup', onWindowMouseUp);

    const _configSpaceMouse = configSpaceMouse(ev);
    if (!_configSpaceMouse) return;
    updateCursor(_configSpaceMouse);
  }

  function renderCanvas() {
    canvasContext.requestFrame();
  }

  function onBeforeFrame() {
    maybeClearInteractionLock();
    resizeOverlayCanvasIfNeeded();
    renderRects();
    renderOverlays();
  }

  onMount(() => {
    window.addEventListener('resize', onWindowResize);
    canvasContext.addBeforeFrameHandler(onBeforeFrame);
  });

  onDestroy(() => {
    window.removeEventListener('resize', onWindowResize);
    canvasContext.removeBeforeFrameHandler(onBeforeFrame);
  });

  let previousFlamechart: any;
  let previousConfigSpaceViewportRect: any;
  let previousCanvasContext: any;
  $: {
    if (flamechart != previousFlamechart) {
      renderCanvas();
      previousFlamechart = flamechart;
    } else if (configSpaceViewportRect != previousConfigSpaceViewportRect) {
      renderCanvas();
      previousConfigSpaceViewportRect = configSpaceViewportRect;
    } else if (canvasContext != previousCanvasContext) {
      if (previousCanvasContext) {
        previousCanvasContext.removeBeforeFrameHandler(onBeforeFrame)
      }
      if (canvasContext) {
        canvasContext.addBeforeFrameHandler(onBeforeFrame)
        canvasContext.requestFrame()
      }
    }
  }
</script>

<div
        class="relative overflow-hidden flex flex-col"
        style={`height: ${Sizes.MINIMAP_HEIGHT}px; borderBottom: ${Sizes.SEPARATOR_HEIGHT}px solid ${theme.fgSecondaryColor};`}
        on:mousedown={onMouseDown}
        on:mousemove={onMouseMove}
        on:mouseleave={onMouseLeave}
        on:wheel={onWheel}
        bind:this={container}
>
    <canvas width={1} height={1} class="w-full h-full absolute top-0 left-0" bind:this={overlayCanvas} />
</div>
