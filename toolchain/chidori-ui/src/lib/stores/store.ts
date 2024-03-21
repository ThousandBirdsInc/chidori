import {invoke} from "@tauri-apps/api/tauri";
import { emit, listen } from '@tauri-apps/api/event'
import {dialog} from "@tauri-apps/api";
import {setProfileGroup} from "@/speedscope/app-state/profile-group";
import {CallTreeProfileBuilder, type FrameInfo} from "@/speedscope/lib/profile";
import {writable} from "svelte/store";
import { type ObservedState } from "@/types/ObservedState";

export const cellsState = writable([])
export const loadedPath = writable("")
export const observeState = writable({})
export const executionGraphState = writable({})
export const definitionGraphState = writable({})

export async function syncDefinitionGraphState() {
  const unlisten = await listen('sync:definitionGraphState', (event) => {
    console.log('sync:definitionGraphState', event.payload)
    try {
      definitionGraphState.set(JSON.parse(event.payload))
    } catch (error) {
      definitionGraphState.set({})
    }
  });
  return unlisten
}

export async function syncExecutionGraphState() {
  const unlisten = await listen('sync:executionGraphState', (event) => {
    console.log('sync:executionGraphState', event.payload)
    executionGraphState.set(event.payload)
  });
  return unlisten
}

export async function syncCellsState() {
  const unlisten = await listen('sync:cellsState', (event) => {
    console.log('sync:cellsState', event.payload)
    cellsState.set(event.payload)
  });
  return unlisten
}

export async function syncObserveState() {
  const unlisten = await listen('sync:observeState', (event) => {
    console.log('sync:observeState', event.payload)
    observeState.set(event.payload)
  });
  return unlisten
}

export function sendPlay() {
  emit('execution:run', { message: 'run' })
  play();
}

export function sendPause() {
  pause();
}

export async function selectDirectory() {
  try {
    const path = await dialog.open({ directory: true, multiple: false });
    if (path) {
      await loadAndWatchDirectory(path as string);
      return [
        await listenForExecutionEvents(),
        await syncObserveState(),
        await syncCellsState(),
        await syncExecutionGraphState(),
        await syncDefinitionGraphState()
      ]
    }
  } catch (error) {
    console.error(`Error selecting directory: ${error}`);
  }
}

export async function moveStateViewToId(id: [number, number]) {
  await invoke('move_state_view_to_id', { id })
}

export async function loadAndWatchDirectory(path: string) {
  await invoke('load_and_watch_directory', { path })
  await getLoadedPath()
}

export function play() {
  return invoke('play', {  })
}

export function pause() {
  return invoke('pause', {  })
}

export async function getLoadedPath() {
  loadedPath.set(await invoke('get_loaded_path', { }))
}

export async function listenForExecutionEvents() {

  function getFrameInfo(key: string): FrameInfo {
    return {
      key: key,
      name: key,
      file: `${key}.ts`,
      line: key.length,
    }
  }

  // Initial generic profile
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

  let c = new CallTreeProfileBuilder();

  let framesById = new Map<string, FrameInfo>();
  let eventBuffer = [];


// Function to process and flush the event buffer
  function flushEventBuffer() {
    if (eventBuffer.length === 0) {
      return;
    }
    for (const event of eventBuffer) {
      if (event.payload?.hasOwnProperty("NewSpan")) {
        const span = event.payload["NewSpan"];
        const id = span.id;
        const info = {
          key: span.name,
          name: span.name,
          file: span.location,
          line: span.line,
          metadata: {
            executionId: span.execution_id
          }
        };
        framesById.set(id, info)
        c.enterFrame(info, span.weight)
      }

      if (event.payload?.hasOwnProperty("Close")) {
        let frameInfo = framesById.get(event.payload["Close"][0]);
        if (frameInfo) {
          c.leaveFrame(frameInfo, event.payload["Close"][1])
        }
      }
    }

    // Attempt to build and log the profile after flushing the buffer
    try {
      const profile = c.build();
      console.log(profile);
      setProfileGroup({
        name: "test",
        indexToView: 0,
        profiles: [profile],
      });
    } catch (e) {
      console.error(e);
    }

    // Clear the buffer after processing
    eventBuffer = [];
  }

  // Start the flush timer to run every 1 seconds
  setInterval(flushEventBuffer, 1000);

// Listener to accumulate events in the buffer
  const unlisten = await listen('execution:events', (event) => {
    eventBuffer.push(event); // Accumulate events in the buffer
  });

  return unlisten
}