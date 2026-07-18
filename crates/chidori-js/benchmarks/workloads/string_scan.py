# Python twin of string_scan.js — per-character string reads via indexing +
# ord(), the tokenizer/parser/hash idiom. Indexed loops (not `for c in s`)
# to mirror the JS charCodeAt/charAt access pattern. ASCII only, so Python's
# len/ord agree with JS's UTF-16 code-unit view.
N = 8192  # string length
ROUNDS = 12
s = ""
for i in range(N):
    s += chr(97 + ((i * 31) % 26))
h = 0
for r in range(ROUNDS):
    for i in range(len(s)):
        h = (h * 31 + ord(s[i])) % 1000000007
# A smaller charAt-style pass so single-character reads are covered too.
vowels = 0
for i in range(len(s)):
    c = s[i]
    if c == "a" or c == "e" or c == "i" or c == "o" or c == "u":
        vowels += 1
print("RESULT=" + str(h + vowels))
