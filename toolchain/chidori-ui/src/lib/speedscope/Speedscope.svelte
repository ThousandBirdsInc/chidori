<script lang="ts">
  import {activeProfile, setProfileGroup,} from "@/speedscope/app-state/profile-group";
    import ChronoFlamechartView from "@/speedscope/ChronoFlamechartView.svelte";
    import GLCanvas from "@/speedscope/GLCanvas.svelte";
  import {CallTreeProfileBuilder, type FrameInfo} from "@/speedscope/lib/profile";
  import {canvasContext} from "@/speedscope/app-state/gl-canvas";
  import {importProfileGroupFromText, importProfilesFromArrayBuffer, importProfilesFromFile} from "@/speedscope/import";
  // import profileData from '../../../sample/profiles/speedscope/0.6.0/two-sampled.speedscope.json';
  // import profileData from '../../../sample/profiles/Chrome/Trace-20240316T150408.json';
  import FlamechartMinimapView from "@/speedscope/FlamechartMinimapView.svelte";


  function getFrameInfo(key: string): FrameInfo {
    return {
      key: key,
      name: key,
      file: `${key}.ts`,
      line: key.length,
    }
  }

  const b = new CallTreeProfileBuilder();
    const fa = getFrameInfo('fa')
  const fb = getFrameInfo('fb')
  const fc = getFrameInfo('fc')
  const fd = getFrameInfo('fd')
  const fe = getFrameInfo('fe')
    const start = Date.now()
    b.enterFrame(fa, 0)
    b.enterFrame(fb, 1)
    b.enterFrame(fd, 3)
    b.leaveFrame(fd, 4)
    b.enterFrame(fc, 4)
    b.leaveFrame(fc, 5)
    b.leaveFrame(fb, 5)
    b.leaveFrame(fa, 5)
    b.enterFrame(fa, 6)
    b.enterFrame(fb, 7)
    b.enterFrame(fb, 8)
    b.leaveFrame(fb, 9)
    b.enterFrame(fe, 9)
    b.leaveFrame(fe, 10)
    b.leaveFrame(fb, 10)
    b.leaveFrame(fa, 11)
    const profile = b.build()
    setProfileGroup({
      name: "test",
      indexToView: 0,
      profiles: [profile],
    });


</script>

<div class="top-0 left-0 bottom-0 right-0 overflow-hidden flex flex-col absolute font-mono text-[20px] leading-[20px]">
    <GLCanvas/>
    <div class="relative flex-1 flex overflow-hidden flex-col">
        <ChronoFlamechartView />
    </div>
</div>
