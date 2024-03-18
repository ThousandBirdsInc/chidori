<script lang="ts">
  import {memoizeByShallowEquality} from "@/speedscope/lib/utils";
  import {CallTreeNode, CallTreeProfileBuilder, Frame, type FrameInfo, type Profile} from "@/speedscope/lib/profile";
  import {canvasContext} from "@/speedscope/app-state/gl-canvas";
  import {Flamechart} from "@/speedscope/lib/flamechart";
  import {
    FlamechartID,
    type ProfileState,
    setProfileGroup,
    activeProfile,
    useFlamechartSetters
  } from "@/speedscope/app-state/profile-group";
  import {
    getCanvasContext,
    getFrameToColorBucket,
    createGetColorBucketForFrame,
    createGetCSSColorForFrame,
    getRowAtlas
  } from "@/speedscope/app-state/getters";
  import FlamechartView from "@/speedscope/FlamechartView.svelte";
  import {lightTheme} from "@/speedscope/themes/light-theme";
  import {getChronoViewFlamechart, getChronoViewFlamechartRenderer} from "@/speedscope/memoizedRenderers";
  import type {FlamechartRenderer} from "@/speedscope/gl/flamechart-renderer";
  import {afterUpdate, beforeUpdate} from "svelte";
  import FlamechartMinimapView from "@/speedscope/FlamechartMinimapView.svelte";

  const theme = lightTheme;

  let flamechart: Flamechart | null = null;
  let flamechartRenderer: FlamechartRenderer | null = null;
  let chronoViewState: any = null;

  // TODO: something is wrong with the initialization of the profile group and its logical space
  $: {
    if ($canvasContext && $activeProfile) {
      chronoViewState = $activeProfile.chronoViewState;
      const profile = $activeProfile.profile;
      const frameToColorBucket = getFrameToColorBucket(profile)
      const getColorBucketForFrame = createGetColorBucketForFrame(frameToColorBucket)
      const getCSSColorForFrame = createGetCSSColorForFrame({theme, frameToColorBucket})
      flamechart = getChronoViewFlamechart({profile, getColorBucketForFrame})
      flamechartRenderer = getChronoViewFlamechartRenderer({
        canvasContext: $canvasContext,
        flamechart,
      })
    }
  }
  const setters = useFlamechartSetters(FlamechartID.CHRONO);
</script>

<div class="h-full w-full relative overflow-hidden flex-col flex text-black">
    {#if $canvasContext && flamechartRenderer && flamechart && chronoViewState}
        <FlamechartView
                theme={theme}
                renderInverted={false}
                flamechart={flamechart}
                flamechartRenderer={flamechartRenderer}
                canvasContext={$canvasContext}
                {...chronoViewState}
                {...setters}
        />
    {/if}
</div>
