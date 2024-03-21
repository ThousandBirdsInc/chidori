<script lang="ts">
  import type {CanvasContext} from "@/speedscope/gl/canvas-context";
  import {glCanvasStore, canvasContext} from "./executionGraphStore";
  import {onDestroy, onMount} from "svelte";

  let glCanvas: HTMLCanvasElement | null;

  let container: HTMLDivElement | null;

  const maybeResize = () => {
    if (!container) return;
    if (!$canvasContext) return;

    let { width, height } = container.getBoundingClientRect();

    const widthInAppUnits = width;
    const heightInAppUnits = height;
    const widthInPixels = width * window.devicePixelRatio;
    const heightInPixels = height * window.devicePixelRatio;

    $canvasContext.gl.resize(
      widthInPixels,
      heightInPixels,
      widthInAppUnits,
      heightInAppUnits
    );
  };

  const onWindowResize = () => {
    if ($canvasContext) {
      maybeResize();
      $canvasContext.requestFrame();
    }
  };

  let prevCanvasContext: CanvasContext | null = null;
  $: {
    if ($canvasContext != prevCanvasContext && $canvasContext) {
      if (prevCanvasContext) {
        $canvasContext.removeBeforeFrameHandler(maybeResize);
      }
      if ($canvasContext) {
        $canvasContext.addBeforeFrameHandler(maybeResize);
        $canvasContext.requestFrame();
      }
      prevCanvasContext = $canvasContext;
    }
  }

  $: {
    if (glCanvas) {
      glCanvasStore.set(glCanvas)
      maybeResize();
    }
  }

  onMount(() => {
    window.addEventListener('resize', onWindowResize);
  });

  onDestroy(() => {
    if ($canvasContext) {
      $canvasContext.removeBeforeFrameHandler(maybeResize);
    }
    window.removeEventListener('resize', onWindowResize);
  });
</script>


<div class="absolute w-full h-full pointer-events-none top-0 left-0" bind:this={container}>
    <canvas height="1" width="1" bind:this={glCanvas} />
</div>
