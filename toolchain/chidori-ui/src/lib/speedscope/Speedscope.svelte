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
  function getRandomFrameId() {
    const frames = ['a1', 'b2', 'c3', 'd4', 'e5'];
    const index = Math.floor(Math.random() * frames.length);
    return frames[index];
  }

  let currentTime = 0;
  function generateFramesWithIncreasingValue(b, lastCumulativeValue, increaseStep = 1) {
    let cumulativeValue = lastCumulativeValue + increaseStep; // Ensure increase from last value
    const stack = [];

    while (true) {
      const action = Math.random() > 0.5; // Randomly decide to enter or leave a frame

      if (stack.length > 0 && action) {
        // Leave a frame
        const { frameId } = stack.pop();
        b.leaveFrame(getFrameInfo(frameId), currentTime);
        cumulativeValue += 1; // Increase cumulative value
      } else if (!action) {
        // Enter a new frame
        const frameId = getRandomFrameId();
        stack.push({ frameId, time: currentTime });
        b.enterFrame(getFrameInfo(frameId), currentTime);
        cumulativeValue += 1; // Increase cumulative value
      }

      currentTime += 1; // Increment time

      // Break the loop if the cumulative value has increased enough
      if (cumulativeValue >= lastCumulativeValue + increaseStep) break;
    }

    // Ensure all frames are closed
    while (stack.length > 0) {
      const { frameId } = stack.pop();
      b.leaveFrame(getFrameInfo(frameId), currentTime);
      currentTime += 1; // Increment time to ensure logical order
      cumulativeValue += 1; // Ensure final increase for closed frames
    }

    return cumulativeValue; // Return the new cumulative value
  }

  const b = new CallTreeProfileBuilder();

  // let lastCumulativeValue = 0;
  // function appendFramesPeriodically() {
  //   lastCumulativeValue = generateFramesWithIncreasingValue(b, lastCumulativeValue, 1); // Increase by at least 1
  // }
  // appendFramesPeriodically();

  // // Periodically append more frames
  // const intervalId = setInterval(() => {
  //   appendFramesPeriodically();
  //
  //   // Stop after a certain condition or time
  //   if (currentTime > 100) { // Example condition
  //     clearInterval(intervalId);
  //     // Use the profile as needed
  //   }
  //   const profile = b.build();
  //   setProfileGroup({
  //     name: "test",
  //     indexToView: 0,
  //     profiles: [profile],
  //   });
  // }, 5000); // Adjust the interval time as needed
  // (async () => {
  //   const profileGroup =  await importProfileGroupFromText("test.json", JSON.stringify(profileData));
  //   if (profileGroup) {
  //     setProfileGroup(profileGroup);
  //   }
  // })();

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
