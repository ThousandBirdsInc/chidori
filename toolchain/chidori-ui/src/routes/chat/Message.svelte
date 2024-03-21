<!-- Message.svelte -->
<script>
    export let message;
</script>

<div class={message.sender === 'user' ? 'text-right' : 'text-left'}>
    <div class="inline-block max-w-xl px-4 py-2 rounded-lg {message.sender === 'user' ? 'bg-blue-500 text-white' : 'bg-gray-200 text-black'}">
        {message.text}
    </div>
</div>
