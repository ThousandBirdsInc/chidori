import * as Icons from "./icons";

import type { Icon } from "lucide-svelte";
import type { ComponentType } from "svelte";


export type Route = {
  title: string;
  label: string;
  icon: ComponentType<Icon>;
  variant: "default" | "ghost";
  path: string;
};


export const primaryRoutes: Route[] = [
  {
    title: "Notebook",
    label: "",
    icon: Icons.FileCode2,
    variant: "default",
    path: "/code",
  },
  {
    title: "Trace",
    label: "",
    icon: Icons.Activity,
    variant: "ghost",
    path: "/trace",
  },
  // {
  //   title: "Logs",
  //   label: "",
  //   icon: Icons.FileText,
  //   variant: "ghost",
  //   path: "/logs",
  // },
  {
    title: "Execution Graph",
    label: "",
    icon: Icons.ScatterChart,
    variant: "ghost",
    path: "/execution_graph",
  },
  {
    title: "Graph",
    label: "",
    icon: Icons.ScatterChart,
    variant: "ghost",
    path: "/graph",
  },
  {
    title: "Chat",
    label: "",
    icon: Icons.MessageSquareText,
    variant: "ghost",
    path: "/chat",
  },
];
