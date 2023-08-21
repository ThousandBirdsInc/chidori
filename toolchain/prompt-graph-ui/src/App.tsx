import { useState } from "react"
import { invoke } from "@tauri-apps/api/tauri"

import { Menu } from "@/components/menu"

import { TailwindIndicator } from "./components/tailwind-indicator"
import { ThemeProvider } from "./components/theme-provider"
import LogsPage from "./logs/page"
import { cn } from "./lib/utils"
import {Navigation} from "@/components/nav";
import {useStore} from "@/stores/store";

function App() {
  const mainNav = useStore(state => state.mainNav)
  const [name, setName] = useState("example")

  let page = undefined;
  switch (mainNav) {
    case "logs":
      page = <LogsPage />
      break;
    case "traces":
      page = <div>Traces</div>
      break;
    case "templates":
      page = <div>Templates</div>
      break;
  }

  return (
    <ThemeProvider attribute="class" defaultTheme="system" enableSystem>
      <div className="h-screen overflow-clip">
        <Menu />
        <div
          className={cn(
            "h-screen overflow-auto border-t bg-background pb-8",
            "scrollbar scrollbar-track-transparent scrollbar-thumb-accent scrollbar-thumb-rounded-md"
          )}
        >
          <div className="flex-col md:flex">
            <Navigation/>
            { page }
          </div>
        </div>
      </div>
      <TailwindIndicator />
    </ThemeProvider>
  )
}

export default App
