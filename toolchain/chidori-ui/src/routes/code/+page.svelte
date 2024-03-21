<script>
    import {cellsState} from "@/stores/store"
    import Cell from "./Cell.svelte";
</script>

<div class="w-full h-full relative flex flex-col gap-4 overflow-y-scroll">
    {#each $cellsState as cell}
        <Cell item={cell}/>
    {/each}
</div>