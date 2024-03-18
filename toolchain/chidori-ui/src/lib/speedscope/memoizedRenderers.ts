import {FlamechartRenderer, type FlamechartRendererOptions} from "@/speedscope/gl/flamechart-renderer";
import {memoizeByShallowEquality} from "@/speedscope/lib/utils";
import type {CanvasContext} from "@/speedscope/gl/canvas-context";
import {Flamechart} from "@/speedscope/lib/flamechart";
import {getRowAtlas} from "@/speedscope/app-state/getters";
import {Frame, type Profile} from "@/speedscope/lib/profile";

const createMemoizedFlamechartRenderer = (options?: FlamechartRendererOptions) =>
  memoizeByShallowEquality(
    ({
       canvasContext,
       flamechart,
     }: {
      canvasContext: CanvasContext
      flamechart: Flamechart
    }): FlamechartRenderer => {
      return new FlamechartRenderer(
        canvasContext.gl,
        getRowAtlas(canvasContext),
        flamechart,
        canvasContext.rectangleBatchRenderer,
        canvasContext.flamechartColorPassRenderer,
        options,
      )
    },
  )

export const getChronoViewFlamechartRenderer = createMemoizedFlamechartRenderer()

export const getChronoViewFlamechart = memoizeByShallowEquality(
  ({
     profile,
     getColorBucketForFrame,
   }: {
    profile: Profile
    getColorBucketForFrame: (frame: Frame) => number
  }): Flamechart => {
    return new Flamechart({
      getTotalWeight: profile.getTotalWeight.bind(profile),
      forEachCall: profile.forEachCall.bind(profile),
      formatValue: profile.formatValue.bind(profile),
      getColorBucketForFrame,
    })
  },
)
