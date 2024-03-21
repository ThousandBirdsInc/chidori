import {derived, type Writable, writable} from 'svelte/store';
import {getCanvasContext} from "@/speedscope/app-state/getters";
import {lightTheme} from "@/speedscope/themes/light-theme";

export const glCanvasStore : Writable<HTMLCanvasElement | null>= writable(null);

export const canvasContext = derived(glCanvasStore, $glCanvasStoreState => {
  if ($glCanvasStoreState == null) return null;
  const cv = getCanvasContext({ theme: lightTheme, canvas: $glCanvasStoreState });
  console.log(cv)
  return cv;
});
