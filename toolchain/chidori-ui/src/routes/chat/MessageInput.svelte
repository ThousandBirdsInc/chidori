<script>
    import { createEventDispatcher } from 'svelte';
    import {Button} from "@/components/ui/button";

    const dispatch = createEventDispatcher();
    let text = '';
    let file = null;
    let fileInput;
    let fileURL = ''; // Variable to store the file URL

    function send() {
        if (text.trim() === '' && !file) return;
        const message = { text, file };
        dispatch('send', message);
        text = '';
        file = null;
        fileInput.value = ''; // Reset the file input
        fileURL = ''; // Clear the file URL after sending
    }

    function handleFileChange(event) {
        const files = event.target.files;
        if (files.length > 0) {
            file = files[0];
            fileURL = URL.createObjectURL(file); // Generate a URL for the file
        }
    }

    function clickFileInput() {
        fileInput.click();
    }
</script>

<style>
    .file-input {
        opacity: 0;
        width: 0.1px;
        height: 0.1px;
        position: absolute;
    }
    .icon-button {
        cursor: pointer;
        display: inline-flex;
        align-items: center;
        justify-content: center;
        padding: 2px;
    }
    .file-preview {
        max-width: 200px; /* Set a max-width for the file preview */
        max-height: 200px; /* Set a max-height for the file preview */
        margin-top: 10px; /* Space between the input box and preview */
    }
</style>

<div class="flex flex-col space-y-2 items-center">
    <div class="flex w-full space-x-2 items-center">
        <input type="file" id="fileInput" class="file-input" on:change={handleFileChange} bind:this={fileInput}/>
        <div class="icon-button" on:click={clickFileInput}>
            <!-- SVG for the clip icon -->
            <svg class="w-6 h-6 text-gray-500" stroke="currentColor" fill="currentColor" stroke-width="0" viewBox="0 0 512 512" height="200px" width="200px" xmlns="http://www.w3.org/2000/svg"><path fill="none" stroke-linecap="round" stroke-miterlimit="10" stroke-width="32" d="M216.08 192v143.85a40.08 40.08 0 0 0 80.15 0l.13-188.55a67.94 67.94 0 1 0-135.87 0v189.82a95.51 95.51 0 1 0 191 0V159.74"></path></svg>
        </div>
        <input class="flex-grow p-2 border rounded" type="text" bind:value={text} placeholder="Type a message..." on:keyup="{e => e.key === 'Enter' && send()}"/>
        <Button size="default" on:click={send}>Send</Button>
    </div>
    {#if fileURL}
        <div class="file-preview">
            <img src={fileURL} alt="File preview" class="object-contain"/>
        </div>
    {/if}
</div>
