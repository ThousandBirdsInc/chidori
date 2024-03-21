<script lang="ts">
  import { Button } from "@/components/ui/button";
  import { cn } from "$lib/utils.js";
  import * as Tooltip from "@/components/ui/tooltip";
  import type { Route } from "@/config";
  import { dialog } from '@tauri-apps/api';
  import { invoke } from '@tauri-apps/api/tauri';
  import { emit, listen } from '@tauri-apps/api/event'
  import {File, Pause, Play} from "@/icons";
  import {loadAndWatchDirectory, selectDirectory, sendPlay, sendPause} from "@/stores/store"
  import { page } from '$app/stores'; // Import the page store
  import {loadedPath} from "@/stores/store";

  const isActiveRoute = (routePath: string) => $page.url.pathname === routePath;
  export let isCollapsed: boolean;
  export let routes: Route[];

</script>

<div data-collapsed={isCollapsed} class="group flex flex-col gap-4 py-2 data-[collapsed=true]:py-2">
    <nav
            class="grid gap-2 px-2 group-[[data-collapsed=true]]:justify-center group-[[data-collapsed=true]]:px-2"
    >

        {#each routes as route}
            {#if isCollapsed}
                <Tooltip.Root openDelay={0}>
                    <Tooltip.Trigger asChild let:builder>
                        <Button
                                href="#"
                                builders={[builder]}
                                variant={isActiveRoute(route.path) ? 'default' : 'ghost'}
                                size="icon"
                                class={cn(
								"size-9",
								(isActiveRoute(route.path) ? 'default' : 'ghost') === "default" &&
									"dark:bg-muted dark:text-muted-foreground dark:hover:bg-muted dark:hover:text-white"
							)}
                        >
                            <svelte:component this={route.icon} class="size-4" aria-hidden="true" />
                            <span class="sr-only">{route.title}</span>
                        </Button>
                    </Tooltip.Trigger>
                    <Tooltip.Content side="right" class="flex items-center gap-4">
                        {route.title}
                        {#if route.label}
							<span class="ml-auto text-muted-foreground">
								{route.label}
							</span>
                        {/if}
                    </Tooltip.Content>
                </Tooltip.Root>
            {:else}
                <Button
                        href={route.path}
                        variant={isActiveRoute(route.path) ? 'default' : 'ghost'}
                        size="sm"
                        class={cn("justify-start", {
						"dark:bg-muted dark:text-white dark:hover:bg-muted dark:hover:text-white":
							(isActiveRoute(route.path) ? 'default' : 'ghost') === "default",
					})}
                >
                    <svelte:component this={route.icon} class="mr-2 size-4" aria-hidden="true" />
                    {route.title}
                    {#if route.label}
						<span
                                class={cn("ml-auto", {
								"text-background dark:text-white": (isActiveRoute(route.path) ? 'default' : 'ghost') === "default",
							})}
                        >
							{route.label}
						</span>
                    {/if}
                </Button>
            {/if}
        {/each}
    </nav>
</div>