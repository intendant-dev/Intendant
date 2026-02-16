use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

pub struct Conversation {
    messages: Vec<Message>,
}

impl Conversation {
    pub fn new(system_prompt: String) -> Self {
        Self {
            messages: vec![Message {
                role: "system".to_string(),
                content: system_prompt,
            }],
        }
    }

    pub fn add_user(&mut self, content: String) {
        self.messages.push(Message {
            role: "user".to_string(),
            content,
        });
    }

    pub fn add_assistant(&mut self, content: String) {
        self.messages.push(Message {
            role: "assistant".to_string(),
            content,
        });
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_conversation_has_system_prompt() {
        let conv = Conversation::new("You are a helpful assistant.".to_string());
        let msgs = conv.messages();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[0].content, "You are a helpful assistant.");
    }

    #[test]
    fn add_user_message() {
        let mut conv = Conversation::new("system".to_string());
        conv.add_user("hello".to_string());
        let msgs = conv.messages();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1].role, "user");
        assert_eq!(msgs[1].content, "hello");
    }

    #[test]
    fn add_assistant_message() {
        let mut conv = Conversation::new("system".to_string());
        conv.add_assistant("response".to_string());
        let msgs = conv.messages();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1].role, "assistant");
        assert_eq!(msgs[1].content, "response");
    }

    #[test]
    fn conversation_ordering() {
        let mut conv = Conversation::new("sys".to_string());
        conv.add_user("msg1".to_string());
        conv.add_assistant("resp1".to_string());
        conv.add_user("msg2".to_string());
        let msgs = conv.messages();
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[1].role, "user");
        assert_eq!(msgs[2].role, "assistant");
        assert_eq!(msgs[3].role, "user");
    }

    #[test]
    fn message_serialization() {
        let msg = Message {
            role: "user".to_string(),
            content: "test".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.role, "user");
        assert_eq!(deserialized.content, "test");
    }
}
