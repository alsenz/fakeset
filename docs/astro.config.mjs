import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

export default defineConfig({
  site: 'https://alsenz.github.io',
  base: '/fakeset',
  integrations: [
    starlight({
      title: 'fakeset',
      description: 'Declarative synthetic dataset generator',
      logo: { src: './src/assets/logo.svg', alt: 'fakeset' },
      customCss: ['./src/styles/custom.css'],
      social: {
        github: 'https://github.com/alsenz/fakeset',
      },
      sidebar: [
        {
          label: 'Getting Started',
          items: [
            { label: 'Introduction', link: '/' },
            { label: 'Installation & Quick Start', link: '/getting-started/' },
          ],
        },
        {
          label: 'Examples',
          items: [
            { label: 'Corporate Registry', link: '/examples/corporate-registry/' },
            { label: 'Insurance', link: '/examples/insurance/' },
          ],
        },
        {
          label: 'Reference',
          items: [
            { label: 'YAML Schema', link: '/reference/yaml-schema/' },
            { label: 'Generators & Locales', link: '/reference/generators/' },
            { label: 'CLI', link: '/reference/cli/' },
            { label: 'Statistical Tests', link: '/reference/testing/' },
          ],
        },
        {
          label: 'Implementation Details',
          items: [
            { label: 'The Semi-Lattice Model', link: '/concepts/semi-lattice/' },
            { label: 'Execution Pipeline', link: '/concepts/execution-pipeline/' },
            { label: 'Bernoulli Factoring', link: '/concepts/bernoulli-factoring/' },
            { label: 'List Links & Witness Assembly', link: '/concepts/list-links/' },
          ],
        },
      ],
    }),
  ],
});
