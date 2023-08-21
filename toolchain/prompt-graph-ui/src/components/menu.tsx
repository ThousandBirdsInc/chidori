"use client"

import { useCallback, useEffect, useState } from "react"
import logo from "@/assets/logo.png"
import { Globe, Mic, Sailboat } from "lucide-react"
import { WindowTitlebar } from "tauri-controls"

import {
  Menubar,
  MenubarCheckboxItem,
  MenubarContent,
  MenubarItem,
  MenubarLabel,
  MenubarMenu,
  MenubarRadioGroup,
  MenubarRadioItem,
  MenubarSeparator,
  MenubarShortcut,
  MenubarSub,
  MenubarSubContent,
  MenubarSubTrigger,
  MenubarTrigger,
} from "@/components/ui/menubar"

import { AboutDialog } from "./about-dialog"
import { MenuModeToggle } from "./menu-mode-toggle"
import { Dialog, DialogTrigger } from "./ui/dialog"

export function Menu() {
  const closeWindow = useCallback(async () => {
    const { appWindow } = await import("@tauri-apps/plugin-window")

    appWindow.close()
  }, [])

  return (
    <WindowTitlebar
      controlsOrder="platform"
      windowControlsProps={{ platform: "macos", className: "" }}
    >
      <Menubar className="rounded-none border-b border-none pl-2 lg:pl-3">
        <MenubarMenu>
          <MenubarTrigger className="font-bold">Chidori</MenubarTrigger>
          <Dialog modal={false}>
            <MenubarContent>
              <DialogTrigger asChild>
                <MenubarItem>About</MenubarItem>
              </DialogTrigger>
              <MenubarSeparator />
              <MenubarItem onClick={closeWindow}>
                Quit <MenubarShortcut>⌘Q</MenubarShortcut>
              </MenubarItem>
            </MenubarContent>
            <AboutDialog />
          </Dialog>
        </MenubarMenu>
        {/*<MenuModeToggle />*/}
      </Menubar>
    </WindowTitlebar>
  )
}
