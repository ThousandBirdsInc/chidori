import type React from 'react'
import remarkGfm from 'remark-gfm'
import { Prism as SyntaxHighlighter } from 'react-syntax-highlighter'
import { prism } from 'react-syntax-highlighter/dist/cjs/styles/prism'
import 'github-markdown-css/github-markdown.css'
import ReactMarkdown from 'react-markdown'
import { CodeProps } from "react-markdown/lib/ast-to-react";
import styles from '@/styles/react-markdown.module.scss'
import React, { SyntheticEvent, useState } from "react";
import { CopyIcon } from '@radix-ui/react-icons'



interface Props {
  children: JSX.Element
  codeText: string
}

const CopyButton: React.FC<Props> = ({ children, codeText }) => {
  const handleClick = (e: SyntheticEvent) => {
    navigator.clipboard.writeText(codeText);
  }

  return (
    <div>
      <span className="text-white absolute right-2 top-1 hover:cursor-pointer transition hover:scale-150">
        <CopyIcon onClick={handleClick} />
      </span>
      {children}
    </div>
  )
}


// Customize the prism object to change the colors used by the syntax highlighting
const customPrism = {
  ...prism,
  background: "#1e1e1e",
  comment: "#666666",
  string: "#00cc00",
  function: "#ffcc00",
  keyword: "#cc00ff",
  className: "#9900cc",
}


interface Props {
  className?: string
  text: string
}

const MarkdownPreview = (props: Props) => {

  const baseProps = {
  }

  const handleClick = (x: any) => {
    console.log(x);
  }

  const handleContentChange = (event: any, node: any, props: any) => {
    console.log(event, node, props);
  }

  const internalString = props.text;

  return <div className={props.className}>
    <ReactMarkdown
      remarkPlugins={[remarkGfm]}
      className={`text-sm ${styles.reactMarkDown} border-2 rounded bg-gray-900 p-2`}
      components={{
        a({ node, ...props}) { return <a {...props} /> },
        blockquote({ node, ...props }) { return <blockquote {...props} /> },
        em({ node, ...props }) { return <em {...props} {...baseProps} className="text-purple-600 font-semibold" /> },
        h1({ node, ...props }) { return <h1 {...props} {...baseProps} className="text-4xl font-bold mb-4" /> },
        h2({ node, ...props }) { return <h2 {...props} {...baseProps} className="text-3xl font-bold mb-3" /> },
        h3({ node, ...props }) { return <h3 {...props} {...baseProps} className="text-2xl font-bold mb-2" /> },
        h4({ node, ...props }) { return <h4 {...props} {...baseProps} className="text-xl font-semibold mb-2" /> },
        h5({ node, ...props }) { return <h5 {...props} {...baseProps} className="text-lg font-semibold mb-1" /> },
        h6({ node, ...props }) { return <h6 {...props} {...baseProps} className="text-base font-semibold mb-1" /> },
        hr({ node, ...props }) { return <hr {...props} {...baseProps} className="border-2 border-gray-300 my-4" /> },
        img({ node, ...props }) { return <img {...props} {...baseProps} className="object-contain w-full h-auto" /> },
        li({ node, ...props }) { return <li {...props} {...baseProps} className="list-disc pl-4 my-2" /> },
        ol({ node, ...props }) { return <ol {...props} {...baseProps} className="list-decimal pl-4 my-2" /> },
        p({ node, ...props }) { return <p {...props} {...baseProps} className="mb-2 leading-relaxed" /> },
        pre({ node, ...props }) { return <pre {...props} {...baseProps} className="p-4 bg-gray-100 overflow-scroll" /> },
        strong({ node, ...props }) { return <strong {...props} {...baseProps} className="font-bold" /> },
        ul({ node, ...props }) { return <ul {...props} {...baseProps} className="list-disc pl-4 my-2" /> },
        code({ node, inline, className, children, style, ...props }: CodeProps) {
          const match = /language-(\w+)/.exec(className || '')
          return !inline && match ? (
            <CopyButton codeText={String(children)}>
              <SyntaxHighlighter
                styles={customPrism}
                language={match[1]}
                PreTag="div"
                {...props}
              >
                {String(children).replace(/\n$/, '')}
              </SyntaxHighlighter>
            </CopyButton>
          ) : (
            <CopyButton codeText={String(children)}>
              <code className={className} {...props}>
                {children}
              </code>
            </CopyButton>
          )
        }
      }}
    >{internalString}</ReactMarkdown>
  </div>
}

export default MarkdownPreview