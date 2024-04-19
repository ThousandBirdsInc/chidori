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

</script>

<div class="relative w-full h-full">
    {JSON.stringify($executionGraphState)}
</div>
