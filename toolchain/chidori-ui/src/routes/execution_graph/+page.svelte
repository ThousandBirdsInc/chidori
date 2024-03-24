<script lang="ts">
  import GraphGLCanvas from "./GraphGLCanvas.svelte";
  import {executionGraphState} from "@/stores/store"
  import {canvasContext} from "./executionGraphStore";
  import {NodeBatch, NodeBatchRenderer} from "./node-batch-renderer";
  import {AffineTransform, Rect, Vec2} from "@/speedscope/lib/math";
  import {Color} from "@/speedscope/lib/color";
  import NodePanZoomView from "./NodePanZoomView.svelte";
  import FlamechartPanZoomView from "@/speedscope/FlamechartPanZoomView.svelte";
  import {lightTheme} from "@/speedscope/themes/light-theme";
  import {Sizes} from "@/speedscope/themes/styles";
  import {CallTreeNode} from "@/speedscope/lib/profile";
  import {formatPercent} from "@/speedscope/lib/utils";
  import {setConfigSpaceViewportRect} from "@/speedscope/app-state/profile-group";
  let container: null | HTMLCanvasElement = null;

  let configSpaceViewportRect = new Rect(new Vec2(0, 0), new Vec2(1, 1));
  let logicalSpaceViewportSize = new Vec2(1, 1);
  function configSpaceSize() {
    return new Vec2(
      1,
      1,
    );
  }

  function _setConfigSpaceViewportRect(viewportRect: Rect): void {
    const configSpaceDetailViewHeight = Sizes.DETAIL_VIEW_HEIGHT / Sizes.FRAME_HEIGHT;
    const width = viewportRect.size.x;
    const size = viewportRect.size.withX(width);
    const origin = viewportRect.origin;
    configSpaceViewportRect = new Rect(origin, viewportRect.size.withX(width))
  }

  function transformViewport(transform: AffineTransform): void {
    const viewportRect = transform.transformRect(configSpaceViewportRect);
    _setConfigSpaceViewportRect(viewportRect);
  }

  function onNodeHover(hover: { node: string; event: MouseEvent } | null) {
    console.log(hover);
  }

  function onNodeClick(node: string | null) {
    console.log(node);
  }

  let selectedNode: string | null = null;
  let nodeBatchRenderer: null | NodeBatchRenderer = null;
  let nodeBatch: null | NodeBatch = null;
  let edges: any;
  $: {
    if ($canvasContext && $executionGraphState) {
      const parsed = JSON.parse($executionGraphState);
      nodeBatch = new NodeBatch($canvasContext.gl);
      parsed.nodes.forEach((node: any) => {
        nodeBatch.addSquare(new Vec2(node.position.x, node.position.y), 20, Color.fromCSSHex("#000000"));
      });
      edges = parsed.edges;
      nodeBatchRenderer = new NodeBatchRenderer($canvasContext.gl)
    }
  }
</script>

<div class="relative w-full h-full">
    {#if $canvasContext && nodeBatchRenderer && nodeBatch}
        <NodePanZoomView
                edges={edges}
                nodes={nodeBatch}
                theme={lightTheme}
                canvasContext={$canvasContext}
                nodeBatchRenderer={nodeBatchRenderer}
                renderInverted={false}
                onNodeHover={onNodeHover}
                onNodeSelect={onNodeClick}
                selectedNode={selectedNode}
                transformViewport={transformViewport}
                configSpaceViewportRect={configSpaceViewportRect}
                setConfigSpaceViewportRect={(x) => configSpaceViewportRect = x}
                logicalSpaceViewportSize={logicalSpaceViewportSize}
                setLogicalSpaceViewportSize={(x) => logicalSpaceViewportSize = x}
        />
    {/if}

    <GraphGLCanvas />
</div>
