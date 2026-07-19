import type { BaseLayoutProps } from 'fumadocs-ui/layouts/shared';

export const REPO_URL = 'https://github.com/ThousandBirdsInc/chidori';

export function baseOptions(): BaseLayoutProps {
  return {
    nav: {
      title: (
        <>
          <svg
            width="18"
            height="18"
            viewBox="0 0 24 24"
            fill="currentColor"
            aria-hidden
          >
            <path d="M13 2 3 14h7l-1 8 10-12h-7l1-8z" />
          </svg>
          Chidori
        </>
      ),
    },
    githubUrl: REPO_URL,
    links: [
      {
        text: 'Examples',
        url: `${REPO_URL}/tree/main/examples`,
        external: true,
      },
      {
        text: 'Discord',
        url: 'https://discord.gg/CJwKsPSgew',
        external: true,
      },
    ],
  };
}
