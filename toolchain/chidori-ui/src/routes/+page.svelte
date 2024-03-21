<script>
    // import { emit, listen } from '@tauri-apps/api/event'
    import { Button } from "@/components/ui/button";
    import {File, Pause, Play} from "@/icons";
    import CodePage from "./code/+page.svelte"
    import {loadAndWatchDirectory, selectDirectory, sendPlay, sendPause, loadedPath} from "@/stores/store"
</script>

<style lang="postcss">
    :global(html) {
        background-color: theme(colors.gray.100);
    }
</style>

{#if $loadedPath}
    <CodePage/>
{:else}
    <div class="w-full h-full flex justify-center align-middle items-center">
        <div class="flex flex-col gap-4">
            <h1>Select a Folder to Execute</h1>
            <Button on:click={selectDirectory} variant="default" size="sm">
                Select Folder&nbsp;<File aria-hidden="true" />
            </Button>
        </div>
    </div>
{/if}