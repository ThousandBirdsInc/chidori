import {immer} from "zustand/middleware/immer";
import {combine, devtools} from "zustand/middleware";
import {create} from "zustand";
import {applyPatches, produce} from "immer";
import {invoke} from "@tauri-apps/api/tauri";
import {ChangeValueWithCounter, NodeWillExecuteOnBranch} from "@/protobufs/DSL_v1"


type MainNav = 'logs' | 'traces' | 'templates' | 'definition'

const store =
  immer(combine({
    mainNav: 'logs' as MainNav,
    nodeWillExecuteEvents: {} as Record<number, NodeWillExecuteOnBranch>,
    changeEvents: {} as Record<number, ChangeValueWithCounter>,
    changes: [] as ChangeValueWithCounter[],

    possibleFiles: [] as string[],
    currentFile: '',
  }, (set, get) => ({
    navigate: (page: MainNav) => {
      set(produce((state) => {
        state.mainNav = page
      }))
    },
    setCurrentFile: (file: string) => {
      set(produce((state) => {
        state.currentFile = file
      }))
    },
    fetchFiles: async () => {
      const files = await invoke("list_files", {});
      set(produce((state) => {
        state.possibleFiles = files
      }))
    },

    fetchInitialState: async () => {
      const initialState = await invoke("get_initial_state", {}) as any;
      set(produce((state) => {
        for (const [key, value] of Object.entries(initialState["change_events"])) {
          state.changeEvents[key] = ChangeValueWithCounter.decode(value as any)
        }
        for (const [key, value] of Object.entries(initialState["node_will_execute_events"])) {
          state.nodeWillExecuteEvents[key] = NodeWillExecuteOnBranch.decode(value as any)
        }
      }))
    },
  })))


interface StoreState {
  mainNav: MainNav,
  nodeWillExecuteEvents: Record<number, NodeWillExecuteOnBranch>,
  changeEvents: Record<number, ChangeValueWithCounter>,
  changes: ChangeValueWithCounter[],
  possibleFiles: string[],
  currentFile: string,
  navigate: (page: MainNav) => void,
  fetchInitialState: () => void,
}


// useEffect(() => {
//   const unlisten = listen<string>('receiveChanges', (event) => {
//     console.log('Received event:', event.payload);
//     setData([...data, event.payload]);
//   });
//
//   return () => {
//     unlisten.then(f => f());
//   };
// }, []);

export const useStore = create<StoreState>(store)
