<script lang="ts">
  import { Button } from "@/components/ui/button";
  import { cn } from "$lib/utils.js";
  import * as Tooltip from "@/components/ui/tooltip";
  import type { Route } from "@/config.ts";
  import { dialog } from '@tauri-apps/api';
  import { invoke } from '@tauri-apps/api/tauri';
  import { emit, listen } from '@tauri-apps/api/event'
  import {File, Pause, Play} from "@/icons";

  export let isCollapsed: boolean;
  export let routes: Route[];

  function handleRun() {
    emit('execution:run', { message: 'run' })
  }

  function runScriptOnDirectory(path) {
    invoke('run_script_on_directory', { path })
      .then(() => console.log('Script execution started'))
      .catch(err => console.error(`Error running script: ${err}`));
  }

  async function selectDirectory() {
    try {
      const path = await dialog.open({ directory: true, multiple: false });
      if (path) {
        runScriptOnDirectory(path);
      }
    } catch (error) {
      console.error(`Error selecting directory: ${error}`);
    }
  }
</script>

<div data-collapsed={isCollapsed} class="group flex flex-col gap-4 py-2 data-[collapsed=true]:py-2">
    <nav
            class="grid gap-2 px-2 group-[[data-collapsed=true]]:justify-center group-[[data-collapsed=true]]:px-2"
    >
        <div class="p-2 flex justify-center gap-2">
            <Button variant="default" size="sm">
                <Play class="h-6" aria-hidden="true" />
            </Button>
            <Button variant="default" size="sm">
                <Pause aria-hidden="true" />
            </Button>
            <Button on:click={selectDirectory} variant="default" size="sm">
                <File aria-hidden="true" />
            </Button>
        </div>

        {#each routes as route}
            {#if isCollapsed}
                <Tooltip.Root openDelay={0}>
                    <Tooltip.Trigger asChild let:builder>
                        <Button
                                href="#"
                                builders={[builder]}
                                variant={route.variant}
                                size="icon"
                                class={cn(
								"size-9",
								route.variant === "default" &&
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
                        variant={route.variant}
                        size="sm"
                        class={cn("justify-start", {
						"dark:bg-muted dark:text-white dark:hover:bg-muted dark:hover:text-white":
							route.variant === "default",
					})}
                >
                    <svelte:component this={route.icon} class="mr-2 size-4" aria-hidden="true" />
                    {route.title}
                    {#if route.label}
						<span
                                class={cn("ml-auto", {
								"text-background dark:text-white": route.variant === "default",
							})}
                        >
							{route.label}
						</span>
                    {/if}
                </Button>
            {/if}
        {/each}
        <div class="p-2 bg-zinc-200 rounded border border-zinc-300">
            This will be a list of running cells
            <ul>
                <li>Item</li>
                <li>Item</li>
                <li>Item</li>
                <li>Item</li>
            </ul>
        </div>
    </nav>
</div>