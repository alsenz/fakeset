# Reframe.

> Claude, just a hint that we're taking our terminology from order theory and partially ordered sets here, and then taking terminology from wider graph theory for the execution DAG. Everything else comes from existing theory - Bernoulli probabiltiy, IFP algorithm alread yused etc etc.

## High level approach

The current fakeset algorithm is correct and well-staged, but its conceptual framing is fragmented across several abstractions that were introduced incrementally: includes, content includes, pool siblings, inner flats, prefills, collects. Each abstraction made local sense at the time but the vocabulary doesn't immediately reveal the unified structure underneath.

The idea is to build an execution DAG by modelling dataset definitions as concepts within a concept semi-lattice with concept inheritance (standard `include`) as the partial order and joint probability sampling as meet or infinum (greatest lower bound - we use '&' as the symbol for brevity).

We model datasets as semi-lattices since they will have multiple 'tops' -- organisations and indivdiuals for example are different concepts that are not related to each other via ordering (inheritance) realtion. In reality, this will start as a union of C disconnected concept semi-lattices, since it's possible two inheritance chains are not connected by links.

### During planning

Initially, the semi lattices only reflect standard inclusion (not link inclusion), and don't model joint distributions between siblings or variants.

First, the elements are expanded to model concepts from the yaml specification of a dataset: nesting, link includes and variants. We do this by creating extra nodes or elements in the semi-lattice structure, not necessarily concrete datasets.

Then, joint distributions are modelled by
i) Taking the set of immediate predecessors (lower covers) of an element - i.e. everything which includes it
ii) Creating 2^n

#### Planning | Building an execution graph from the semi-lattice structure

As the final stage of planning, the semi-lattice structure is converted into an actual execution graph. This doesn't preserve the ordering relation perfectly but extends it into a structure represented by a DAG.

The primary reason why the union of semi-lattices becomes a DAG is the introduction of *new edges* (which could be thought of as pairwise extension of the ordering relation) which nevertheless changes the infinum to no longer be joint distribution.

> Question for claude: after we've added links to ensure correct execution order in the DAG between linked dataset relations, what *is* the infinum (&) and is it useful to us?

#### During execution



### High level flow

> Claude: could you please draw some short mermaid diagrams showing these graphs at each stage and how we extend and manipulate them.

*Planning*

1. *Initial set*. Read all the yamls as elements of the set, including files which do not produce output.
   1. An ordering relation is induced on the `include` hierarchy.
   2. For now, this is a poset of N distinct semi-lattices for each include hierarchy. 

2. *Variants*. Elements with variant fields in their definitions are expended. 
   1. 
   2. For now, includes ratios for the element are adjusted proportionately to the 'ratio' field in the original variant element field definition.


*Execution*


# Glossary

> Clause, please write a glossary of the key terms of our conceptual framing. For example:

For example:
Inheritance: A inherits from B if A's dataset definition has an `include` stanza referencing B. For example, the dataset of cats might inherit from the dataset of animals. Generally, taken as an ordering relation, cats are executed first.

## Concept map

> Claude, please write a table mapping YAML field features to the new conceptual framing, mapped to perhaps some additional implementation details