<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { Rect, AffineTransform, Vec2, clamp } from '@/speedscope/lib/math';
  import {Flamechart, type FlamechartFrame} from '@/speedscope/lib/flamechart';
  import { CanvasContext } from '@/speedscope/gl/canvas-context';
  import {NodeBatch, NodeBatchRenderer} from './node-batch-renderer';
  import { ProfileSearchResults } from '@/speedscope/lib/profile-search';
  import { type Theme } from '@/speedscope/themes/theme';
  import {FontFamily, FontSize, Sizes} from '@/speedscope/themes/styles';
  import {BatchCanvasTextRenderer, BatchCanvasRectRenderer} from '@/speedscope/lib/canvas-2d-batch-renderers'
  import {cachedMeasureTextWidth, ELLIPSIS, remapRangesToTrimmedText, trimTextMid} from "@/speedscope/lib/text-utils";
  import {Color} from "@/speedscope/lib/color";
  import {getCanvasContext} from "@/speedscope/app-state/getters";
  import {lightTheme} from "@/speedscope/themes/light-theme";

  export let nodes: NodeBatch;
  export let canvasContext: CanvasContext;
  export let nodeBatchRenderer: NodeBatchRenderer;
  export let renderInverted: boolean;
  export let selectedNode: string | null;
  export let theme: Theme;
  export let onNodeHover: (hover: {node: string; event: MouseEvent} | null) => void;
  export let onNodeSelect: (node: string | null) => void;
  export let configSpaceViewportRect: Rect;
  export let transformViewport: (transform: AffineTransform) => void;
  export let setConfigSpaceViewportRect: (rect: Rect) => void;
  export let logicalSpaceViewportSize: Vec2;
  export let setLogicalSpaceViewportSize: (size: Vec2) => void;

  let hoveredLabel: {node: string; configSpaceBounds: Rect} | null = null;

  let container: HTMLDivElement;
  let overlayCanvas: HTMLCanvasElement;
  let overlayCtx: CanvasRenderingContext2D | null;

  const LOGICAL_VIEW_SPACE_FRAME_HEIGHT = Sizes.FRAME_HEIGHT;

  function configSpaceSize() {
    return new Vec2(
      overlayCanvas.width,
      overlayCanvas.height
    );
  }

  function physicalViewSize() {
    if (!overlayCanvas) return new Vec2(0, 0);
    const rect = overlayCanvas.getBoundingClientRect();
    return new Vec2(rect.width * window.devicePixelRatio, rect.height * window.devicePixelRatio);
  }

  function physicalBounds() {
    if (!overlayCanvas) return new Rect(new Vec2(0, 0), new Vec2(0, 0));
    const offset = new Vec2(0, 0); // Canvas offset
    return new Rect(offset, physicalViewSize());
  }

  function configSpaceToPhysicalViewSpace() {
    return AffineTransform.betweenRects(configSpaceViewportRect, physicalBounds());
  }

  function logicalToPhysicalViewSpace() {
    return AffineTransform.withScale(new Vec2(window.devicePixelRatio, window.devicePixelRatio));
  }

  function resizeOverlayCanvasIfNeeded() {
    if (!overlayCanvas) return;
    let {width, height} = overlayCanvas.getBoundingClientRect();
    /*
    We render text at a higher resolution then scale down to
    ensure we're rendering at 1:1 device pixel ratio.
    This ensures our text is rendered crisply.
    */

    width = Math.floor(width);
    height = Math.floor(height);

    // Still initializing: don't resize yet
    if (width === 0 || height === 0) return;

    const scaledWidth = width * window.devicePixelRatio;
    const scaledHeight = height * window.devicePixelRatio;

    if (scaledWidth === overlayCanvas.width && scaledHeight === overlayCanvas.height)
      return;

    overlayCanvas.width = scaledWidth;
    overlayCanvas.height = scaledHeight;
  }

  function renderOverlays() {
    const ctx = overlayCtx;
    if (!ctx) return;
    if (configSpaceViewportRect.isEmpty()) return;

    const configToPhysical = configSpaceToPhysicalViewSpace();

    const physicalViewSpaceFontSize = FontSize.LABEL * window.devicePixelRatio;
    const physicalViewSpaceFrameHeight =
      LOGICAL_VIEW_SPACE_FRAME_HEIGHT * window.devicePixelRatio;

    const _physicalViewSize = physicalViewSize();

    ctx.clearRect(0, 0, _physicalViewSize.x, _physicalViewSize.y);

    ctx.font = `${physicalViewSpaceFontSize}px/${physicalViewSpaceFrameHeight}px ${FontFamily.MONOSPACE}`;
    ctx.textBaseline = 'alphabetic';

    const minWidthToRender = cachedMeasureTextWidth(ctx, 'M' + ELLIPSIS + 'M')
    const minConfigSpaceWidthToRender = (
      configToPhysical.inverseTransformVector(new Vec2(minWidthToRender, 0)) || new Vec2(0, 0)
    ).x

    const LABEL_PADDING_PX = 5 * window.devicePixelRatio

    const labelBatch = new BatchCanvasTextRenderer()
    const fadedLabelBatch = new BatchCanvasTextRenderer()
    const matchedTextHighlightBatch = new BatchCanvasRectRenderer()
    const directlySelectedOutlineBatch = new BatchCanvasRectRenderer()
    const indirectlySelectedOutlineBatch = new BatchCanvasRectRenderer()
    const matchedFrameBatch = new BatchCanvasRectRenderer()

    const frameOutlineWidth = 2 * window.devicePixelRatio
    ctx.strokeStyle = theme.selectionSecondaryColor
    const minConfigSpaceWidthToRenderOutline = (
      configToPhysical.inverseTransformVector(new Vec2(1, 0)) || new Vec2(0, 0)
    ).x

    const renderSpecialFrameOutlines = (frame: FlamechartFrame, depth = 0) => {
      if (!selectedNode) return
      const width = frame.end - frame.start
      const y = renderInverted ? configSpaceSize().y - 1 - depth : depth
      const configSpaceBounds = new Rect(new Vec2(frame.start, y), new Vec2(width, 1))

      if (width < minConfigSpaceWidthToRenderOutline) return
      if (configSpaceBounds.left() > configSpaceViewportRect.right()) return
      if (configSpaceBounds.right() < configSpaceViewportRect.left()) return
      if (configSpaceBounds.top() > configSpaceViewportRect.bottom()) return

      if (configSpaceBounds.hasIntersectionWith(configSpaceViewportRect)) {
        // if (selectedNode != null && frame.node.frame === selectedNode.frame) {
        //   let batch =
        //     frame.node === selectedNode
        //       ? directlySelectedOutlineBatch
        //       : indirectlySelectedOutlineBatch
        //
        //   const physicalRectBounds = configToPhysical.transformRect(configSpaceBounds)
        //   batch.rect({
        //     x: Math.round(physicalRectBounds.left() + 1 + frameOutlineWidth / 2),
        //     y: Math.round(physicalRectBounds.top() + 1 + frameOutlineWidth / 2),
        //     w: Math.round(Math.max(0, physicalRectBounds.width() - 2 - frameOutlineWidth)),
        //     h: Math.round(Math.max(0, physicalRectBounds.height() - 2 - frameOutlineWidth)),
        //   })
        // }
      }
      for (let child of frame.children) {
        renderSpecialFrameOutlines(child, depth + 1)
      }
    }

    // for (let frame of flamechart.getLayers()[0] || []) {
    //   renderSpecialFrameOutlines(frame)
    // }
    //
    // for (let frame of flamechart.getLayers()[0] || []) {
    //   renderFrameLabelAndChildren(frame)
    // }

    matchedFrameBatch.fill(ctx, theme.searchMatchPrimaryColor)
    matchedTextHighlightBatch.fill(ctx, theme.searchMatchSecondaryColor)
    fadedLabelBatch.fill(ctx, theme.fgSecondaryColor)
    labelBatch.fill(
      ctx,
      theme.fgPrimaryColor,
    )
    indirectlySelectedOutlineBatch.stroke(ctx, theme.selectionSecondaryColor, frameOutlineWidth)
    directlySelectedOutlineBatch.stroke(ctx, theme.selectionPrimaryColor, frameOutlineWidth)

    if (hoveredLabel) {
      let color: string = theme.fgPrimaryColor
      if (selectedNode === hoveredLabel.node) {
        color = theme.selectionPrimaryColor
      }

      ctx.lineWidth = 2 * devicePixelRatio
      ctx.strokeStyle = color

      const physicalViewBounds = configToPhysical.transformRect(hoveredLabel.configSpaceBounds)
      ctx.strokeRect(
        Math.round(physicalViewBounds.left()),
        Math.round(physicalViewBounds.top()),
        Math.round(Math.max(0, physicalViewBounds.width())),
        Math.round(Math.max(0, physicalViewBounds.height())),
      )
    }
  }

  function updateConfigSpaceViewport() {
    if (!container) return;
    const {width, height} = container.getBoundingClientRect();

    // Still initializing: don't resize yet
    if (width < 2 || height < 2) return;

    if (configSpaceViewportRect.isEmpty()) {
      const configSpaceViewportHeight = height / LOGICAL_VIEW_SPACE_FRAME_HEIGHT;
      if (renderInverted) {
        setConfigSpaceViewportRect(
          new Rect(
            new Vec2(0, configSpaceSize().y - configSpaceViewportHeight + 1),
            new Vec2(configSpaceSize().x, configSpaceViewportHeight),
          ),
        );
      } else {
        setConfigSpaceViewportRect(
          new Rect(new Vec2(0, -1), new Vec2(configSpaceSize().x, configSpaceViewportHeight)),
        );
      }
    } else if (
      !logicalSpaceViewportSize.equals(Vec2.zero) &&
      (logicalSpaceViewportSize.x !== width || logicalSpaceViewportSize.y !== height)
    ) {
      // Resize the viewport rectangle to match the window size aspect
      // ratio.
      setConfigSpaceViewportRect(
        configSpaceViewportRect.withSize(
          configSpaceViewportRect.size.timesPointwise(
            new Vec2(width / logicalSpaceViewportSize.x, height / logicalSpaceViewportSize.y),
          ),
        ),
      );
    }

    const newSize = new Vec2(width, height);
    if (!newSize.equals(logicalSpaceViewportSize)) {
      setLogicalSpaceViewportSize(newSize);
    }
  }

  function onWindowResize() {
    updateConfigSpaceViewport()
    onBeforeFrame()
  }

  function renderRects() {
    if (!container) return;
    updateConfigSpaceViewport();

    if (configSpaceViewportRect.isEmpty()) return;

    canvasContext.renderBehind(container, () => {
      nodeBatchRenderer.render({
        batch: nodes,
        physicalSpaceDstRect: physicalBounds(),
        configSpaceSrcRect: configSpaceViewportRect,
      });
    });
  }

  // Inertial scrolling introduces tricky interaction problems.
  // Namely, if you start panning, and hit the edge of the scrollable
  // area, the browser continues to receive WheelEvents from inertial
  // scrolling. If we start zooming by holding Cmd + scrolling, then
  // release the Cmd key, this can cause us to interpret the incoming
  // inertial scrolling events as panning. To prevent this, we introduce
  // a concept of an "Interaction Lock". Once a certain interaction has
  // begun, we don't allow the other type of interaction to begin until
  // we've received two frames with no inertial wheel events. This
  // prevents us from accidentally switching between panning & zooming.
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

  function onBeforeFrame() {
    resizeOverlayCanvasIfNeeded();
    renderRects();
    renderOverlays();
    maybeClearInteractionLock();
  }

  function renderCanvas() {
    canvasContext.requestFrame();
  }

  function pan(logicalViewSpaceDelta: Vec2) {
    interactionLock = 'pan';

    const physicalDelta = logicalToPhysicalViewSpace().transformVector(logicalViewSpaceDelta);
    const configDelta = configSpaceToPhysicalViewSpace().inverseTransformVector(physicalDelta);

    if (hoveredLabel) {
      onNodeHover(null);
    }

    if (!configDelta) return;
    transformViewport(AffineTransform.withTranslation(configDelta));
  }

  function zoom(logicalViewSpaceCenter: Vec2, multiplier: number) {
    interactionLock = 'zoom';

    const physicalCenter = logicalToPhysicalViewSpace().transformPosition(
      logicalViewSpaceCenter,
    );
    const configSpaceCenter = configSpaceToPhysicalViewSpace().inverseTransformPosition(
      physicalCenter,
    );
    if (!configSpaceCenter) return;

    const zoomTransform = AffineTransform.withTranslation(configSpaceCenter.times(-1))
      .scaledBy(new Vec2(multiplier, multiplier))
      .translatedBy(configSpaceCenter);

    transformViewport(zoomTransform);
  }

  let lastDragPos: Vec2 | null = null;
  let mouseDownPos: Vec2 | null = null;
  function onMouseDown(ev: MouseEvent) {
    mouseDownPos = lastDragPos = new Vec2(ev.offsetX, ev.offsetY);
    updateCursor();
    window.addEventListener('mouseup', onWindowMouseUp);
  }

  function onMouseDrag(ev: MouseEvent) {
    if (!lastDragPos) return;
    const logicalMousePos = new Vec2(ev.offsetX, ev.offsetY);
    pan(lastDragPos.minus(logicalMousePos));
    lastDragPos = logicalMousePos;

    if (hoveredLabel) {
      onNodeHover(null);
    }
  }

  function onDblClick(ev: MouseEvent) {
    if (hoveredLabel) {
      const hoveredBounds = hoveredLabel.configSpaceBounds;
      const viewportRect = new Rect(
        hoveredBounds.origin.minus(new Vec2(0, 1)),
        hoveredBounds.size.withY(configSpaceViewportRect.height()),
      );
      setConfigSpaceViewportRect(viewportRect);
    }
  }

  function onClick(ev: MouseEvent) {
    const logicalMousePos = new Vec2(ev.offsetX, ev.offsetY);
    mouseDownPos = mouseDownPos;
    mouseDownPos = null;

    if (mouseDownPos && logicalMousePos.minus(mouseDownPos).length() > 5) {
      return;
    }

    if (hoveredLabel) {
      onNodeSelect(hoveredLabel.node);
      renderCanvas();
    } else {
      onNodeSelect(null);
    }
  }

  function updateCursor() {
    if (lastDragPos) {
      document.body.style.cursor = 'grabbing';
      document.body.style.cursor = '-webkit-grabbing';
    } else {
      document.body.style.cursor = 'default';
    }
  }

  function onWindowMouseUp(ev: MouseEvent) {
    lastDragPos = null;
    updateCursor();
    window.removeEventListener('mouseup', onWindowMouseUp);
  }

  function onMouseMove(ev: MouseEvent) {
    updateCursor();
    if (lastDragPos) {
      ev.preventDefault();
      onMouseDrag(ev);
      return;
    }
    hoveredLabel = null;
    const logicalViewSpaceMouse = new Vec2(ev.offsetX, ev.offsetY);
    const physicalViewSpaceMouse = logicalToPhysicalViewSpace().transformPosition(
      logicalViewSpaceMouse,
    );
    const configSpaceMouse = configSpaceToPhysicalViewSpace().inverseTransformPosition(
      physicalViewSpaceMouse,
    );

    if (!configSpaceMouse) return;

    const setHoveredLabel = (frame: FlamechartFrame, depth = 0) => {
      const width = frame.end - frame.start
      const y = renderInverted ? configSpaceSize().y - 1 - depth : depth
      const configSpaceBounds = new Rect(new Vec2(frame.start, y), new Vec2(width, 1))
      if (configSpaceMouse.x < configSpaceBounds.left()) return null
      if (configSpaceMouse.x > configSpaceBounds.right()) return null

      // if (configSpaceBounds.contains(configSpaceMouse)) {
      //   hoveredLabel = {
      //     configSpaceBounds,
      //     node: frame.node,
      //   }
      // }

      for (let child of frame.children) {
        setHoveredLabel(child, depth + 1)
      }
    }

    // for (let frame of flamechart.getLayers()[0] || []) {
    //   setHoveredLabel(frame)
    // }

    if (hoveredLabel) {
      // @ts-ignore
      onNodeHover({node: hoveredLabel!.node, event: ev})
    } else {
      onNodeHover(null)
    }

    renderCanvas()

  }

  function onMouseLeave(ev: MouseEvent) {
    hoveredLabel = null;
    onNodeHover(null);
    renderCanvas();
  }

  function onWheel(ev: WheelEvent) {
    ev.preventDefault();
    frameHadWheelEvent = true;

    const isZoom = ev.metaKey || ev.ctrlKey;

    let deltaY = ev.deltaY;
    let deltaX = ev.deltaX;
    if (ev.deltaMode === ev.DOM_DELTA_LINE) {
      deltaY *= LOGICAL_VIEW_SPACE_FRAME_HEIGHT;
      deltaX *= LOGICAL_VIEW_SPACE_FRAME_HEIGHT;
    }

    if (isZoom && interactionLock !== 'pan') {
      let multiplier = 1 + deltaY / 100;

      if (ev.ctrlKey) {
        multiplier = 1 + deltaY / 40;
      }

      multiplier = clamp(multiplier, 0.1, 10.0);

      zoom(new Vec2(ev.offsetX, ev.offsetY), multiplier);
    } else if (interactionLock !== 'zoom') {
      pan(new Vec2(deltaX, deltaY));
    }

    renderCanvas();
  }

  function onWindowKeyPress(ev: KeyboardEvent) {
    if (!container) return;
    const {width, height} = container.getBoundingClientRect();

    if (ev.key === '=' || ev.key === '+') {
      zoom(new Vec2(width / 2, height / 2), 0.5);
      ev.preventDefault();
    } else if (ev.key === '-' || ev.key === '_') {
      zoom(new Vec2(width / 2, height / 2), 2);
      ev.preventDefault();
    }

    if (ev.ctrlKey || ev.shiftKey || ev.metaKey) return;

    if (ev.key === '0') {
      zoom(new Vec2(width / 2, height / 2), 1e9);
    } else if (ev.key === 'ArrowRight' || ev.code === 'KeyD') {
      pan(new Vec2(100, 0));
    } else if (ev.key === 'ArrowLeft' || ev.code === 'KeyA') {
      pan(new Vec2(-100, 0));
    } else if (ev.key === 'ArrowUp' || ev.code === 'KeyW') {
      pan(new Vec2(0, -100));
    } else if (ev.key === 'ArrowDown' || ev.code === 'KeyS') {
      pan(new Vec2(0, 100));
    } else if (ev.key === 'Escape') {
      onNodeSelect(null);
      renderCanvas();
    }
  }

  $: {
    if (overlayCanvas) {
      overlayCtx = overlayCanvas.getContext('2d')
      renderCanvas();
    }
  }

  let previousSelectedNode: any;
  let previousConfigSpaceViewportRect: any;
  let previousCanvasContext: any;
  $: {
    if (selectedNode != previousSelectedNode) {
      renderCanvas();
      previousSelectedNode = selectedNode;
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

  onMount(() => {
    canvasContext.addBeforeFrameHandler(onBeforeFrame);
    window.addEventListener('resize', onWindowResize);
    window.addEventListener('keydown', onWindowKeyPress);
  });

  onDestroy(() => {
    canvasContext.removeBeforeFrameHandler(onBeforeFrame);
    window.removeEventListener('resize', onWindowResize);
    window.removeEventListener('keydown', onWindowKeyPress);
  });
</script>

<div
        class="w-full h-full overflow-hidden flex flex-col relative top-0 left-0"
        on:mousedown={onMouseDown}
        on:mousemove={onMouseMove}
        on:mouseleave={onMouseLeave}
        on:click={onClick}
        on:dblclick={onDblClick}
        on:wheel={onWheel}
        bind:this={container}
>
    <canvas width={1} height={1} class="w-full h-full absolute top-0 left-0" bind:this={overlayCanvas} />
</div>