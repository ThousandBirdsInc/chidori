<script lang="ts">
  import { onMount } from 'svelte';

  import {CallTreeNode, Frame} from './lib/profile';
  import { Rect, Vec2, AffineTransform } from './lib/math';
  import { formatPercent } from './lib/utils';
  import { Sizes } from './themes/styles';
  import { type Theme } from './themes/theme';
  import FlamechartPanZoomView from './FlamechartPanZoomView.svelte';
  import type {CanvasContext} from "@/speedscope/gl/canvas-context";
  import type {Flamechart} from "@/speedscope/lib/flamechart";
  import type {FlamechartRenderer} from "@/speedscope/gl/flamechart-renderer";
  import FlamechartMinimapView from "@/speedscope/FlamechartMinimapView.svelte";
  // import type { FlamechartViewProps } from './flamechart-view-container';

  interface FlamechartSetters {
    setLogicalSpaceViewportSize: (logicalSpaceViewportSize: Vec2) => void
    setConfigSpaceViewportRect: (configSpaceViewportRect: Rect) => void
    setNodeHover: (hover: {node: CallTreeNode; event: MouseEvent} | null) => void
    setSelectedNode: (node: CallTreeNode | null) => void
  }

  interface FlamechartViewState {
    hover: {
      node: CallTreeNode
      event: MouseEvent
    } | null
    selectedNode: CallTreeNode | null
    logicalSpaceViewportSize: Vec2
    configSpaceViewportRect: Rect
  }


  type FlamechartViewProps = {
    theme: Theme
    canvasContext: CanvasContext
    flamechart: Flamechart
    flamechartRenderer: FlamechartRenderer
    renderInverted: boolean
    getCSSColorForFrame: (frame: Frame) => string
  } & FlamechartSetters &
    FlamechartViewState


  export let renderInverted: boolean;
  export let theme: FlamechartViewProps['theme'];
  export let canvasContext: FlamechartViewProps['canvasContext'];
  export let flamechart: FlamechartViewProps['flamechart'];
  export let flamechartRenderer: FlamechartViewProps['flamechartRenderer'];
  export let selectedNode: FlamechartViewProps['selectedNode'];
  export let configSpaceViewportRect: FlamechartViewProps['configSpaceViewportRect'];
  export let logicalSpaceViewportSize: FlamechartViewProps['logicalSpaceViewportSize'];
  export let setConfigSpaceViewportRect: FlamechartViewProps['setConfigSpaceViewportRect'];
  export let setLogicalSpaceViewportSize: FlamechartViewProps['setLogicalSpaceViewportSize'];
  export let setNodeHover: FlamechartViewProps['setNodeHover'];
  export let setSelectedNode: FlamechartViewProps['setSelectedNode'];
  export let hover: FlamechartViewProps['hover'];

  let container: HTMLDivElement | null = null;


  function configSpaceSize() {
    return new Vec2(
      flamechart.getTotalWeight(),
      flamechart.getLayers().length,
    );
  }

  function _setConfigSpaceViewportRect(viewportRect: Rect): void {
    const configSpaceDetailViewHeight = Sizes.DETAIL_VIEW_HEIGHT / Sizes.FRAME_HEIGHT;

    const width = flamechart.getClampedViewportWidth(viewportRect.size.x);
    const size = viewportRect.size.withX(width);

    const origin = Vec2.clamp(
      viewportRect.origin,
      new Vec2(0, -1),
      Vec2.max(
        Vec2.zero,
        configSpaceSize().minus(size).plus(new Vec2(0, configSpaceDetailViewHeight + 1)),
      ),
    );

    setConfigSpaceViewportRect(new Rect(origin, viewportRect.size.withX(width)));
  }

  function transformViewport(transform: AffineTransform): void {
    const viewportRect = transform.transformRect(configSpaceViewportRect);
    _setConfigSpaceViewportRect(viewportRect);
  }

  function onNodeHover(hover: { node: CallTreeNode; event: MouseEvent } | null) {
    setNodeHover(hover);
  }

  function onNodeClick(node: CallTreeNode | null) {
    setSelectedNode(node);
  }

  function formatValue(weight: number) {
    const totalWeight = flamechart.getTotalWeight();
    const percent = (100 * weight) / totalWeight;
    const formattedPercent = formatPercent(percent);
    return `${flamechart.formatValue(weight)} (${formattedPercent})`;
  }
</script>

<div
    class="h-full w-full relative overflow-hidden flex-col flex"
    bind:this={container}>
    <FlamechartMinimapView
            theme={theme}
            flamechart={flamechart}
            flamechartRenderer={flamechartRenderer}
            canvasContext={canvasContext}
            setConfigSpaceViewportRect={_setConfigSpaceViewportRect}
            transformViewport={transformViewport}
            configSpaceViewportRect={configSpaceViewportRect}
    />
    <FlamechartPanZoomView
            theme={theme}
            canvasContext={canvasContext}
            flamechart={flamechart}
            flamechartRenderer={flamechartRenderer}
            renderInverted={false}
            onNodeHover={onNodeHover}
            onNodeSelect={onNodeClick}
            selectedNode={selectedNode}
            transformViewport={transformViewport}
            configSpaceViewportRect={configSpaceViewportRect}
            setConfigSpaceViewportRect={_setConfigSpaceViewportRect}
            logicalSpaceViewportSize={logicalSpaceViewportSize}
            setLogicalSpaceViewportSize={setLogicalSpaceViewportSize}
            searchResults={null}
    />
    <!--    <FlamechartSearchView />-->
    <div class="w-full h-80">
        {#if container && hover}
            {formatValue(hover.node.getTotalWeight())}
            {hover.node.frame.name}
            {#if hover.node.frame.file}
                <div>
                    {hover.node.frame.file}:{hover.node.frame.line}
                </div>
            {/if}
            {JSON.stringify(hover.node.frame)}
        {/if}
    </div>
</div>