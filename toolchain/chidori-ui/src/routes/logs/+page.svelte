<script>
    // import { emit, listen } from '@tauri-apps/api/event'
    import {onMount} from "svelte";
    import Speedscope from "@/speedscope/Speedscope.svelte";
    //
    // let events = [];
    // onMount(async () => {
    //     // listen to the `click` event and get a function to remove the event listener
    //     // there's also a `once` function that subscribes to an event and automatically unsubscribes the listener on the first event
    //     const unlisten = await listen('event-name', (event) => {
    //         // event.event is the event name (useful if you want to use a single callback fn for multiple event types)
    //         // event.payload is the payload object
    //         events = [...events, event]
    //     })
    //
    //     // emits the `click` event with the object payload
    //     emit('click', {
    //         theMessage: 'Tauri is awesome!',
    //     })
    //     return () => {
    //         unlisten();
    //     }
    // });
</script>

<style lang="postcss">
    :global(html) {
        background-color: theme(colors.gray.100);
    }
</style>

<div class="w-full h-full relative">
    Logs
</div>