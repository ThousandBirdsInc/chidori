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
    title: "Trace",
    label: "128",
    icon: Icons.Activity,
    variant: "default",
    path: "/trace",
  },
  {
    title: "Logs",
    label: "9",
    icon: Icons.FileText,
    variant: "ghost",
    path: "/logs",
  },
  {
    title: "State",
    label: "9",
    icon: Icons.Server,
    variant: "ghost",
    path: "/state",
  },
  {
    title: "Graph",
    label: "9",
    icon: Icons.Server,
    variant: "ghost",
    path: "/graph",
  },
];
