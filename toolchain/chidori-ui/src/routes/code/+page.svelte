<script>
    import {cellsState, moveStateViewToId} from "@/stores/store"
    import Cell from "./Cell.svelte";
    import {Button} from "@/components/ui/button";
</script>

<div class="w-full h-full relative flex flex-col gap-4 overflow-y-scroll">
    <div class="self-start">
        <Button on:click={() => moveStateViewToId([0,0])}>Revert State To (0,0)</Button>
    </div>
    {#each $cellsState as cell}
        <Cell item={cell}/>
    {/each}
</div>