<script lang="ts">
    import CodeComponent from './Code.svelte';
    import PromptComponent from './Prompt.svelte';
    import WebComponent from './Web.svelte';
    import TemplateComponent from './Template.svelte';
    export let item: any = {};
    import {moveStateViewToId, observeState} from "@/stores/store";
    import {Button} from "@/components/ui/button";

    $: cell = item["cell"];
    $: opId = item["op_id"];
    $: appliedAt = item["applied_at"];

    let mostRecentStateId = null;
    let state = null;
    $: {
      if ($observeState[opId]) {
        mostRecentStateId = $observeState[opId][0];
        state = $observeState[opId][1];
      } else {
        mostRecentStateId = null;
        state = null;
      }
    }
</script>

<div class="max-w-2xl p-6 bg-white rounded-lg border  flex flex-col gap-4">
    {JSON.stringify({opId, appliedAt})}
    {#if cell.hasOwnProperty('Code')}
        <CodeComponent cell={cell['Code']} />
    {:else if cell.hasOwnProperty('Prompt')}
        <PromptComponent cell={cell['Prompt']['Chat']} />
    {:else if cell.hasOwnProperty('Web')}
        <WebComponent cell={cell['Web']} />
    {:else if cell.hasOwnProperty('Template')}
        <TemplateComponent cell={cell['Template']} />
    {/if}
    {#if mostRecentStateId}
        <div>
            <Button on:click={() => moveStateViewToId(mostRecentStateId)}>Revert To {mostRecentStateId}</Button>
            {JSON.stringify(state)}
        </div>
    {:else}
        Pending Execution
    {/if}
</div>
