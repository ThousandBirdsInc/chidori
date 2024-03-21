<script lang="ts">
  import * as Menubar from "$lib/components/ui/menubar";

  let isPlaying = false;
  import {loadAndWatchDirectory, selectDirectory, sendPlay, sendPause} from "@/stores/store"
  import {loadedPath} from "@/stores/store";
  import {Pause, Play} from "@/icons";
  import {Button} from "@/components/ui/button";
</script>

<div class="flex flex-row h-10 w-screen items-center justify-between space-x-1 border bg-background p-1">
    <Menubar.Root class="border-none">
        <Menubar.Menu>
            <Menubar.Trigger>File</Menubar.Trigger>
            <Menubar.Content>
                <Menubar.Item on:click={selectDirectory}>
                    Open
                    <Menubar.Shortcut>⌘O</Menubar.Shortcut>
                </Menubar.Item>
                <Menubar.Separator />
                <Menubar.Item>Close</Menubar.Item>
            </Menubar.Content>
        </Menubar.Menu>
    </Menubar.Root>
    <div class="flex items-center text-sm">
        {$loadedPath}
    </div>
    <div class="flex justify-center text-sm">
        <div class="p-2 flex justify-center gap-2">
            {#if $loadedPath}
                {#if isPlaying}
                    <Button on:click={() => {sendPause(); isPlaying = false;}} variant="default" size="sm">
                        <Pause aria-hidden="true" />
                    </Button>
                {:else}
                    <Button on:click={() => {sendPlay(); isPlaying = true;}} variant="default" size="sm">
                        <Play class="h-6" aria-hidden="true" />
                    </Button>
                {/if}
            {/if}
        </div>
    </div>
</div>
