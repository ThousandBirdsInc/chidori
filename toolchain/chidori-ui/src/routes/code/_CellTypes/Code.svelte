<script>
    import CodeEditor from "@/components/CodeEditor.svelte";

    export let cell;
</script>

<div class="flex flex-col gap-4">
    <div class="flex flex-row gap-4">
        <h3 class="font-semibold text-gray-800">Name: <span class="">{cell.name}</span></h3>
        <h3 class="font-semibold text-gray-800">Language: <span class="">{cell.language}</span></h3>
    </div>
    <div class="p-4 bg-gray-100 rounded">
        <CodeEditor initialText={cell.source_code}/>
    </div>
    {#if cell.function_invocation}
        <div class="mt-4">
            <p class="text-md text-gray-600">Function Invocation: <span class="font-mono text-gray-700">{cell.function_invocation}</span></p>
        </div>
    {/if}
</div>