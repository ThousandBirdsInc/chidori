<script lang="ts">
    import Message from './Message.svelte';
    import MessageInput from './MessageInput.svelte';
    let messages = [
        { id: 1, text: "Initial message example.", sender: "bot" },
    ];
    async function handleSend(newMessage: { detail: { text: string } }) {
      const text = newMessage.detail.text;
      messages = [...messages, { id: messages.length + 1, text: text, sender: "user" }];
    }


</script>

<style lang="postcss">
    :global(html) {
        background-color: theme(colors.gray.100);
    }
</style>

<div class="w-full h-full relative">
    <div class="flex flex-col max-w-screen-md">
        <div class="flex-grow overflow-auto p-4 space-y-2">
            {#each messages as message}
                <Message {message} />
            {/each}
        </div>
        <div class="p-2">
            <MessageInput on:send={handleSend} />
        </div>
    </div>

</div>